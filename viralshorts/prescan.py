"""Preseleccion de zonas calientes ANTES de transcribir.

Transcribir un podcast de tres horas cuesta unos cien minutos de CPU, y el 98%
de ese trabajo se tira: al final solo se usan tres tramos de un minuto.

Las dos senales mas fuertes del pipeline -- el heatmap de YouTube y la energia
del audio -- no necesitan texto y se calculan en segundos sobre el video
entero. Este modulo las usa para decidir DONDE mirar, de modo que Whisper solo
transcriba esas zonas. El texto sigue estando disponible donde importa (elegir
el corte exacto en frontera de frase, subtitular), pero el coste baja de horas
a minutos.
"""

from __future__ import annotations

import logging
from typing import List, Optional, Sequence, Tuple

log = logging.getLogger(__name__)

Region = Tuple[float, float]


def choose_regions(
    duration: float,
    heatmap: Sequence["object"],
    energy: Optional["object"],
    cfg_prescan,
    cfg_clips,
    cfg_scoring,
) -> List[Region]:
    """Devuelve tramos [(inicio, fin)] ordenados y sin solaparse."""
    usable_start = cfg_clips.head_skip
    usable_end = duration - cfg_clips.tail_skip
    if usable_end - usable_start < cfg_clips.min_duration:
        usable_start, usable_end = 0.0, duration

    if not heatmap and energy is None:
        log.info("Sin heatmap ni energia: se transcribe el video entero.")
        return [(usable_start, usable_end)]

    span = cfg_clips.max_duration
    step = max(cfg_prescan.step, 1.0)

    # 1. Barrido grueso: puntuamos cada ventana posible con las senales baratas.
    scored: List[tuple[float, float, float, float]] = []
    start = usable_start
    while start + span <= usable_end:
        end = start + span
        heat = _heat_for(heatmap, start, end)
        level = energy.window_score(start, end) if energy is not None else 0.0
        scored.append((start, end, heat, level))
        start += step

    if not scored:
        return [(usable_start, usable_end)]

    weights = {"heat": cfg_scoring.weight_heatmap, "energy": cfg_scoring.weight_energy}
    ranges = {
        "heat": (min(r[2] for r in scored), max(r[2] for r in scored)),
        "energy": (min(r[3] for r in scored), max(r[3] for r in scored)),
    }
    ranked = sorted(scored, key=lambda row: -_combined(row[2], row[3], weights, ranges))

    # 2. Nos quedamos con las mejores sin solaparse, con hueco suficiente para
    #    que luego puedan salir clips distintos.
    wanted = cfg_prescan.regions or max(12, cfg_clips.num_clips * 5)
    picked: List[Region] = []
    for win_start, win_end, _, _ in ranked:
        if len(picked) >= wanted:
            break
        if any(win_start < p_end and win_end > p_start for p_start, p_end in picked):
            continue
        picked.append((win_start, win_end))

    # 3. Margen a cada lado y fusion de los que se toquen.
    pad = cfg_prescan.padding
    padded = sorted(
        (max(usable_start, s - pad), min(usable_end, e + pad)) for s, e in picked
    )
    regions: List[Region] = []
    for region_start, region_end in padded:
        if regions and region_start <= regions[-1][1]:
            regions[-1] = (regions[-1][0], max(regions[-1][1], region_end))
        else:
            regions.append((region_start, region_end))

    covered = sum(e - s for s, e in regions)
    log.info("Preseleccion: %d zonas, %s de audio a transcribir de %s (%.0f%% del total).",
             len(regions), _fmt(covered), _fmt(duration), 100 * covered / max(duration, 1e-6))
    for region_start, region_end in regions:
        log.debug("  zona %s-%s", _fmt(region_start), _fmt(region_end))
    return regions


def _combined(heat: float, level: float, weights, ranges) -> float:
    """Suma ponderada con cada senal normalizada 0..1 sobre la poblacion.

    Una senal plana (sin heatmap, por ejemplo) no aporta informacion, asi que
    se ignora y su peso se reparte entre las demas -- mismo criterio que en
    scoring._weighted.
    """
    total = total_weight = 0.0
    for name, value in (("heat", heat), ("energy", level)):
        weight = weights[name]
        if weight <= 0:
            continue
        low, high = ranges[name]
        if high - low < 1e-9:
            continue
        total += weight * (value - low) / (high - low)
        total_weight += weight
    return total / total_weight if total_weight else 0.0


def _heat_for(heatmap: Sequence["object"], start: float, end: float) -> float:
    if not heatmap:
        return 0.0
    accumulated = total_weight = 0.0
    for point in heatmap:
        overlap = min(end, point.end) - max(start, point.start)
        if overlap <= 0:
            continue
        accumulated += point.value * overlap
        total_weight += overlap
    return accumulated / total_weight if total_weight > 0 else 0.0


def _fmt(seconds: float) -> str:
    minutes, secs = divmod(int(seconds), 60)
    hours, minutes = divmod(minutes, 60)
    return f"{hours}h{minutes:02d}m" if hours else f"{minutes}m{secs:02d}s"
