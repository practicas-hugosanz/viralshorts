"""Seleccion de los momentos del podcast.

El criterio original (densidad de palabras) premia a quien habla mas rapido,
no al momento que engancha. Aqui se combinan cuatro senales independientes:

  1. heatmap  - el "most replayed" de YouTube. Es literalmente la audiencia
                diciendo que parte rebobina. La senal individual mas fuerte.
  2. energia  - risas, subidas de voz, discusiones (ver audio_energy).
  3. densidad - palabras por segundo. Peso bajo: descarta silencios largos,
                poco mas.
  4. Claude   - lee la transcripcion y puntua contenido y gancho.

Las ventanas candidatas se generan SIEMPRE cortando en frontera de frase, de
modo que ningun clip empieza ni acaba a media palabra.
"""

from __future__ import annotations

import bisect
import logging
import math
import re
from collections import Counter
from dataclasses import dataclass, field
from typing import Dict, List, Optional, Sequence, Tuple

from .transcribe import Transcript, Word

log = logging.getLogger(__name__)

Ranges = Dict[str, Tuple[float, float]]


@dataclass
class Window:
    """Un tramo candidato, alineado a frontera de frase."""
    start: float
    end: float
    words: List[Word] = field(default_factory=list)
    text: str = ""

    heat: float = 0.0
    energy: float = 0.0
    density: float = 0.0
    context: float = 0.0
    llm_content: float = 0.0
    llm_hook: float = 0.0
    reason: str = ""
    score: float = 0.0

    @property
    def duration(self) -> float:
        return self.end - self.start

    def overlaps(self, other: "Window", gap: float = 0.0) -> bool:
        return self.start < other.end + gap and self.end > other.start - gap

    def describe(self) -> str:
        parts = [
            f"{hms(self.start)}-{hms(self.end)} ({self.duration:.0f}s)",
            f"total {self.score:.3f}",
            f"heat {self.heat:.2f}",
            f"energia {self.energy:.2f}",
            f"contexto {self.context:.2f}",
        ]
        if self.llm_content or self.llm_hook:
            parts.append(f"contenido {self.llm_content:.2f}")
            parts.append(f"gancho {self.llm_hook:.2f}")
        return " | ".join(parts)


# ----------------------------------------------------------------------------
# 1. Generacion de candidatos
# ----------------------------------------------------------------------------

def build_windows(transcript: Transcript, cfg,
                  regions: Optional[Sequence[Tuple[float, float]]] = None) -> List[Window]:
    """Genera ventanas candidatas cortando en frontera de frase.

    Los puntos de corte son los finales de frase (y, como respaldo, los finales
    de segmento de Whisper), de modo que un clip nunca arranca ni acaba a mitad
    de una palabra.

    Si se transcribio por zonas (ver prescan), `regions` marca los tramos con
    texto: una ventana que empezara en una zona y acabara en otra se saltaria
    todo el hueco intermedio sin enterarse, asi que se descartan.
    """
    boundaries = _sentence_boundaries(transcript)
    if len(boundaries) < 2:
        raise RuntimeError("La transcripcion no tiene suficientes frases para cortar clips.")

    words = transcript.words
    midpoints = [(w.start + w.end) / 2 for w in words]

    usable_start = cfg.head_skip
    usable_end = transcript.duration - cfg.tail_skip
    if usable_end - usable_start < cfg.min_duration:
        # Video mas corto que los margenes configurados: recortar la intro y la
        # despedida dejaria cero candidatos, asi que se ignoran los margenes.
        log.warning("El video dura %.0fs y los margenes (%.0fs + %.0fs) no dejan "
                    "sitio para un clip de %.0fs; se ignoran los margenes.",
                    transcript.duration, cfg.head_skip, cfg.tail_skip,
                    cfg.min_duration)
        usable_start, usable_end = 0.0, transcript.duration

    region_of = _region_index(regions)

    windows: List[Window] = []
    for i, start in enumerate(boundaries[:-1]):
        if start < usable_start or start > usable_end:
            continue
        start_region = region_of(start)
        if start_region is None:
            continue
        lo = bisect.bisect_left(midpoints, start)
        for end in boundaries[i + 1:]:
            length = end - start
            if length < cfg.min_duration:
                continue
            if length > cfg.max_duration or end > usable_end:
                break
            if region_of(end) != start_region:
                break
            hi = bisect.bisect_left(midpoints, end)
            chunk = words[lo:hi]
            if len(chunk) < 10:
                continue
            windows.append(Window(
                start=start, end=end, words=chunk,
                text=" ".join(w.text.strip() for w in chunk).strip(),
            ))

    log.info("Generadas %d ventanas candidatas entre %s y %s.",
             len(windows), hms(usable_start), hms(usable_end))
    if not windows:
        raise RuntimeError(
            "No se pudo generar ninguna ventana candidata. Prueba a bajar "
            "clips.min_duration o clips.head_skip, o a subir clips.max_duration."
        )
    return windows


def _region_index(regions: Optional[Sequence[Tuple[float, float]]]):
    """Devuelve una funcion instante -> indice de zona (None si esta fuera)."""
    if not regions:
        return lambda _t: 0
    ordered = sorted(regions)
    starts = [r[0] for r in ordered]

    def index_at(moment: float) -> Optional[int]:
        i = bisect.bisect_right(starts, moment) - 1
        if i < 0 or moment > ordered[i][1]:
            return None
        return i

    return index_at


def _sentence_boundaries(transcript: Transcript) -> List[float]:
    points = {0.0}
    for segment in transcript.segments:
        points.add(segment.end)
        for word in segment.words:
            if word.ends_sentence:
                points.add(word.end)
    return sorted(points)


# ----------------------------------------------------------------------------
# 2. Senales baratas
# ----------------------------------------------------------------------------

def apply_cheap_signals(
    windows: Sequence[Window],
    heatmap: Sequence["object"],
    energy: Optional["object"],
) -> None:
    """Rellena heat / energy / density en cada ventana (in place)."""
    for window in windows:
        window.heat = _heat_for(heatmap, window.start, window.end)
        window.energy = energy.window_score(window.start, window.end) if energy else 0.0
        window.density = len(window.words) / max(window.duration, 1e-6)


def _heat_for(heatmap: Sequence["object"], start: float, end: float) -> float:
    """Media del heatmap en [start, end), ponderada por solape temporal."""
    if not heatmap:
        return 0.0

    total_weight = 0.0
    accumulated = 0.0
    for point in heatmap:
        overlap = min(end, point.end) - max(start, point.start)
        if overlap <= 0:
            continue
        accumulated += point.value * overlap
        total_weight += overlap
    return accumulated / total_weight if total_weight > 0 else 0.0


# ----------------------------------------------------------------------------
# 3. Combinacion y seleccion
# ----------------------------------------------------------------------------

def shortlist_for_llm(windows: Sequence[Window], limit: int, cfg_scoring) -> List[Window]:
    """Mejores ventanas por senales baratas, sin solaparse entre si.

    Sin el filtro de solape mandariamos al LLM veinte versiones del mismo
    momento desplazadas dos segundos, que es tirar el dinero.
    """
    weights = {
        "heat": cfg_scoring.weight_heatmap,
        "energy": cfg_scoring.weight_energy,
        "density": cfg_scoring.weight_density,
        "context": cfg_scoring.weight_context,
    }
    ranges = compute_ranges(windows, weights)
    ranked = sorted(windows, key=lambda w: -_weighted(w, weights, ranges))

    picked: List[Window] = []
    for window in ranked:
        if len(picked) >= limit:
            break
        if any(window.overlaps(other) for other in picked):
            continue
        picked.append(window)

    picked.sort(key=lambda w: w.start)
    return picked


def apply_llm_scores(windows: Sequence[Window], scores: Dict[int, "object"]) -> None:
    for index, score in scores.items():
        windows[index].llm_content = score.content
        windows[index].llm_hook = score.hook
        windows[index].reason = score.reason


def finalise(windows: Sequence[Window], cfg_scoring, cfg_clips,
             limit: Optional[int] = None) -> List[Window]:
    """Puntua definitivamente y elige los mejores clips sin solaparse.

    `limit` manda sobre `clips.num_clips`: al preparar una seleccion para
    revisar a mano interesa proponer mas candidatos de los que se van a
    renderizar.
    """
    weights = {
        "heat": cfg_scoring.weight_heatmap,
        "energy": cfg_scoring.weight_energy,
        "density": cfg_scoring.weight_density,
        "context": cfg_scoring.weight_context,
        "llm_content": cfg_scoring.weight_llm,
        "llm_hook": cfg_scoring.weight_hook,
    }
    ranges = compute_ranges(windows, weights)
    for window in windows:
        window.score = _weighted(window, weights, ranges)

    wanted = cfg_clips.num_clips if limit is None else limit
    max_similarity = getattr(cfg_scoring, "max_similarity", 1.0)

    picked: List[Window] = []
    fingerprints: List[Counter] = []
    for window in sorted(windows, key=lambda w: -w.score):
        if len(picked) >= wanted:
            break
        if any(window.overlaps(other, gap=cfg_clips.min_gap) for other in picked):
            continue
        # Dos clips del mismo tema, aunque esten en minutos distintos, son el
        # mismo video contado dos veces: el segundo no aporta nada al feed.
        if max_similarity < 1.0:
            fingerprint = _fingerprint(window.text)
            twin = next((i for i, other in enumerate(fingerprints)
                         if _cosine(fingerprint, other) > max_similarity), None)
            if twin is not None:
                log.debug("Descartado %s por repetir el tema de %s.",
                          hms(window.start), hms(picked[twin].start))
                continue
            fingerprints.append(fingerprint)
        picked.append(window)

    picked.sort(key=lambda w: w.start)
    return picked


# ----------------------------------------------------------------------------
# Parecido tematico entre dos clips
# ----------------------------------------------------------------------------

_ACCENTS = str.maketrans("áéíóúüÁÉÍÓÚÜ", "aeiouuaeiouu")

# Palabras que aparecen en todos los clips y no dicen de que va ninguno.
_STOPWORDS = frozenset("""
que para pero como cuando donde porque entonces tambien aunque desde hasta
sobre entre todo toda todos todas nada nadie mucho mucha muy mas menos entre
entonces claro bueno vale digo dice decir hacer hecho tiene tienen tener
estaba estan estoy esta este esta esto eso esa ese esos esas aqui alli ahora
siempre nunca luego despues antes ellos ellas nosotros vosotros usted ustedes
""".split())


def _fingerprint(text: str) -> "Counter":
    """Vocabulario con peso de un clip, sin muletillas ni palabras vacias."""
    words = [
        w for w in re.findall(r"\w+", text.lower().translate(_ACCENTS), re.UNICODE)
        if len(w) >= 4 and w not in _STOPWORDS
    ]
    return Counter(words)


def _cosine(a: "Counter", b: "Counter") -> float:
    if not a or not b:
        return 0.0
    shared = set(a) & set(b)
    if not shared:
        return 0.0
    dot = sum(a[w] * b[w] for w in shared)
    norm_a = math.sqrt(sum(v * v for v in a.values()))
    norm_b = math.sqrt(sum(v * v for v in b.values()))
    return dot / (norm_a * norm_b) if norm_a and norm_b else 0.0


def compute_ranges(population: Sequence[Window], weights: Dict[str, float]) -> Ranges:
    return {
        name: (min(values), max(values)) if (values := [getattr(w, name) for w in population]) else (0.0, 0.0)
        for name in weights
    }


def _weighted(window: Window, weights: Dict[str, float], ranges: Ranges) -> float:
    """Suma ponderada de las senales, cada una normalizada 0..1 en la poblacion.

    Normalizar por poblacion evita que una senal con rango pequeno (p. ej. el
    heatmap de un video muy plano) domine o desaparezca por accidente. Una
    senal constante -- porque no hay heatmap, o porque el LLM no respondio --
    no aporta informacion, asi que se ignora y su peso se reparte entre las
    demas en vez de contar como cero.
    """
    total_weight = 0.0
    total = 0.0
    for name, weight in weights.items():
        if weight <= 0:
            continue
        low, high = ranges[name]
        if high - low < 1e-9:
            continue
        total += weight * (getattr(window, name) - low) / (high - low)
        total_weight += weight

    return total / total_weight if total_weight else 0.0


def hms(seconds: float) -> str:
    minutes, secs = divmod(int(seconds), 60)
    hours, minutes = divmod(minutes, 60)
    if hours:
        return f"{hours}:{minutes:02d}:{secs:02d}"
    return f"{minutes}:{secs:02d}"
