"""Eliminacion de silencios (jump cuts).

Un podcast tiene pausas de uno o dos segundos cada pocas frases. En un video de
45 segundos eso son 5-8 segundos de nada, y en vertical el espectador se va.
Aqui recortamos los silencios largos y, muy importante, devolvemos ademas la
funcion que traslada cualquier instante del clip original al clip ya recortado:
sin ella los subtitulos quedarian desincronizados.
"""

from __future__ import annotations

import logging
from dataclasses import dataclass
from typing import List, Sequence, Tuple

from .transcribe import Word

log = logging.getLogger(__name__)

Interval = Tuple[float, float]


@dataclass
class JumpCutPlan:
    """Tramos que se conservan, en la linea de tiempo del clip original."""
    keeps: List[Interval]
    source_duration: float

    @property
    def enabled(self) -> bool:
        return len(self.keeps) > 1

    @property
    def output_duration(self) -> float:
        return sum(end - start for start, end in self.keeps)

    @property
    def removed(self) -> float:
        return self.source_duration - self.output_duration

    def remap(self, t: float) -> float:
        """Traslada un instante del clip original al clip ya recortado."""
        elapsed = 0.0
        for start, end in self.keeps:
            if t < start:
                return elapsed                      # cae en un hueco eliminado
            if t <= end:
                return elapsed + (t - start)
            elapsed += end - start
        return elapsed

    def remap_words(self, words: Sequence[Word]) -> List[Word]:
        """Reajusta las marcas de tiempo de las palabras al clip recortado."""
        out: List[Word] = []
        for word in words:
            start = self.remap(word.start)
            end = self.remap(word.end)
            if end - start < 0.02:
                # La palabra cayo entera dentro de un hueco eliminado; le damos
                # una duracion minima para que no desaparezca del subtitulo.
                end = start + 0.08
            out.append(Word(word.text, start, end, word.probability))
        return out


def plan(words: Sequence[Word], duration: float, cfg) -> JumpCutPlan:
    """Calcula que tramos del clip se conservan.

    `words` debe venir ya en coordenadas relativas al clip (0 = inicio).
    """
    full = JumpCutPlan(keeps=[(0.0, duration)], source_duration=duration)
    if not cfg.enabled or not words:
        return full

    # 1. Un intervalo por palabra, con margen para no cortar ataques ni colas.
    spoken = [
        (max(0.0, w.start - cfg.padding), min(duration, w.end + cfg.padding))
        for w in words
    ]
    spoken = _merge(spoken)

    # 2. Conservamos hasta `max_silence` de silencio a cada lado de cada bloque
    #    hablado: quitar las pausas del todo suena atropellado e inhumano.
    half = cfg.max_silence / 2.0
    keeps = _merge([
        (max(0.0, start - half), min(duration, end + half))
        for start, end in spoken
    ])

    # 3. Fuera los trozos residuales demasiado cortos para verse como un plano.
    keeps = [(a, b) for a, b in keeps if b - a >= cfg.min_keep]
    if not keeps:
        log.warning("Los jump cuts se comerian el clip entero; se desactivan aqui.")
        return full

    result = JumpCutPlan(keeps=keeps, source_duration=duration)
    if result.removed < 0.5:
        # Menos de medio segundo no compensa el coste extra de filtrado.
        return full

    log.info("Jump cuts: %d tramos, se quitan %.1fs de silencio (%.0f%% -> %.1fs).",
             len(keeps), result.removed,
             100 * result.removed / duration, result.output_duration)
    return result


def _merge(intervals: Sequence[Interval]) -> List[Interval]:
    if not intervals:
        return []
    ordered = sorted(intervals)
    merged = [list(ordered[0])]
    for start, end in ordered[1:]:
        if start <= merged[-1][1]:
            merged[-1][1] = max(merged[-1][1], end)
        else:
            merged.append([start, end])
    return [(a, b) for a, b in merged]
