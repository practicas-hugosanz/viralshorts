"""Curva de energia del audio.

Sirve como proxy barato de "aqui pasa algo": risas, subidas de voz, gente
hablandose encima, golpes de mesa. Un tramo con energia alta y con picos suele
funcionar mejor que uno plano, aunque se digan las mismas palabras.
"""

from __future__ import annotations

import logging
from dataclasses import dataclass
from pathlib import Path
from typing import Optional

log = logging.getLogger(__name__)

_SAMPLE_RATE = 16000
_HOP_SECONDS = 0.25


@dataclass
class EnergyTrack:
    """Nivel RMS normalizado (0..1) muestreado cada `hop` segundos."""
    level: "object"       # np.ndarray float32, 0..1
    hop: float
    duration: float
    # Instante del video al que corresponde la primera muestra. Es 0 cuando la
    # curva cubre el video entero, y el inicio del clip cuando solo cubre un
    # tramo (ver analyse_range).
    offset: float = 0.0

    def window_score(self, start: float, end: float) -> float:
        """Puntua un tramo combinando nivel medio y densidad de picos."""
        import numpy as np

        lo = max(0, int(start / self.hop))
        hi = min(len(self.level), int(end / self.hop))
        if hi - lo < 2:
            return 0.0

        chunk = self.level[lo:hi]
        mean_level = float(np.mean(chunk))
        # Picos = fraccion de tiempo por encima del percentil 80 global. Un
        # tramo con muchos picos tiene mas "movimiento" que uno alto pero plano.
        spikes = float(np.mean(chunk > self._peak_threshold))
        return 0.6 * mean_level + 0.4 * spikes

    def at(self, moment: float) -> float:
        """Nivel (0..1) en un instante concreto. Fuera de rango devuelve 0."""
        index = int((moment - self.offset) / self.hop)
        if index < 0 or index >= len(self.level):
            return 0.0
        return float(self.level[index])

    @property
    def _peak_threshold(self) -> float:
        import numpy as np
        if not hasattr(self, "_cached_threshold"):
            self._cached_threshold = float(np.percentile(self.level, 80))
        return self._cached_threshold


def analyse(path: Path, duration: float) -> EnergyTrack:
    """Extrae la curva de energia del audio completo de un fichero."""
    return _analyse(path, duration, start=None, span=None, quiet=False)


def analyse_range(path: Path, start: float, duration: float) -> EnergyTrack:
    """Curva de energia de un solo tramo, sin decodificar el video entero.

    La usa el reencuadre cuando monta desde un plan guardado: ahi no se ha
    analizado el audio completo, y decodificar tres horas para saber cuando
    calla la gente en cuarenta segundos no tiene sentido.
    """
    return _analyse(path, duration, start=start, span=duration, quiet=True)


def _analyse(path: Path, duration: float, start: Optional[float],
             span: Optional[float], quiet: bool) -> EnergyTrack:
    import numpy as np

    from .ffmpeg_utils import decode_audio_mono

    if not quiet:
        log.info("Analizando la energia del audio...")
    samples = decode_audio_mono(path, sample_rate=_SAMPLE_RATE,
                                start=start, duration=span)

    hop_samples = int(_SAMPLE_RATE * _HOP_SECONDS)
    frames = len(samples) // hop_samples
    if frames < 2:
        if not quiet:
            log.warning("Audio demasiado corto para analizar la energia.")
        return EnergyTrack(level=np.zeros(1, dtype="float32"), hop=_HOP_SECONDS,
                           duration=duration, offset=start or 0.0)

    blocks = samples[: frames * hop_samples].reshape(frames, hop_samples)
    rms = np.sqrt(np.mean(np.square(blocks, dtype="float64"), axis=1))
    # En dB la percepcion de volumen es mucho mas lineal que en amplitud.
    db = 20.0 * np.log10(np.maximum(rms, 1e-6))

    # Normalizamos con percentiles en vez de min/max: un solo golpe de
    # microfono no debe aplastar toda la curva.
    lo, hi = np.percentile(db, 5), np.percentile(db, 97)
    level = np.clip((db - lo) / max(hi - lo, 1e-6), 0.0, 1.0).astype("float32")

    if not quiet:
        log.info("Energia calculada: %d muestras (%.2fs por muestra).",
                 frames, _HOP_SECONDS)
    return EnergyTrack(level=level, hop=_HOP_SECONDS, duration=duration,
                       offset=start or 0.0)
