"""Transcripcion con faster-whisper: UNA sola pasada para todo el pipeline.

El resultado (palabras con marca de tiempo individual) se usa para dos cosas a
la vez: elegir los momentos del podcast y generar los subtitulos. Transcribir
dos veces era la mayor perdida de tiempo del pipeline original.

El resultado se cachea en disco: volver a lanzar el pipeline sobre el mismo
video con la misma configuracion no vuelve a transcribir.
"""

from __future__ import annotations

import hashlib
import json
import logging
import time
from dataclasses import asdict, dataclass, field
from pathlib import Path
from typing import List, Optional, Sequence

from . import progress

log = logging.getLogger(__name__)

_SENTENCE_ENDINGS = {".", "!", "?", "…"}


@dataclass
class Word:
    text: str            # texto tal cual lo devuelve Whisper, con puntuacion
    start: float
    end: float
    probability: float = 1.0

    @property
    def ends_sentence(self) -> bool:
        return self.text.strip()[-1:] in _SENTENCE_ENDINGS

    def shifted(self, offset: float) -> "Word":
        return Word(self.text, self.start + offset, self.end + offset, self.probability)


@dataclass
class Segment:
    start: float
    end: float
    text: str
    words: List[Word] = field(default_factory=list)


@dataclass
class Transcript:
    language: str
    duration: float
    segments: List[Segment]

    @property
    def words(self) -> List[Word]:
        return [w for s in self.segments for w in s.words]

    def words_between(self, start: float, end: float) -> List[Word]:
        """Palabras cuyo centro cae dentro de [start, end)."""
        return [w for w in self.words if start <= (w.start + w.end) / 2 < end]

    def text_between(self, start: float, end: float) -> str:
        return " ".join(w.text.strip() for w in self.words_between(start, end)).strip()


# ----------------------------------------------------------------------------

class Transcriber:
    """Carga el modelo una vez y lo reutiliza para todos los videos."""

    def __init__(self, cfg, cache_dir: Path) -> None:
        self.cfg = cfg
        self.cache_dir = Path(cache_dir)
        self.cache_dir.mkdir(parents=True, exist_ok=True)
        self._model = None
        self._batched = None
        self._device: Optional[str] = None

    # -- modelo ---------------------------------------------------------
    def _load_model(self):
        if self._model is not None:
            return self._model

        try:
            from faster_whisper import WhisperModel
        except ImportError as exc:
            raise ImportError(
                "Falta faster-whisper. Ejecuta: pip install faster-whisper"
            ) from exc

        device, compute_type = self._resolve_device()
        threads = self._cpu_threads()
        log.info("Cargando Whisper '%s' en %s/%s (%d hilos)...",
                 self.cfg.model_size, device, compute_type, threads)
        started = time.perf_counter()
        try:
            self._model = WhisperModel(
                self.cfg.model_size, device=device, compute_type=compute_type,
                cpu_threads=threads,
            )
        except Exception as exc:
            if device == "cpu":
                raise
            # Un fallo tipico aqui es no tener cuDNN/CUDA instalados aunque haya
            # GPU: seguir en CPU es mejor que abortar el pipeline entero.
            log.warning("No se pudo usar la GPU (%s). Reintentando en CPU.", exc)
            self._model = WhisperModel(self.cfg.model_size, device="cpu",
                                       compute_type="int8", cpu_threads=threads)
            device, compute_type = "cpu", "int8"

        self._device = device
        log.info("Modelo listo en %.1fs.", time.perf_counter() - started)
        return self._model

    def _cpu_threads(self) -> int:
        """Hilos para ctranslate2.

        La libreria usa 4 por defecto, lo que en un procesador de 6 nucleos
        deja un tercio de la CPU parada. Usamos los nucleos fisicos: pasar de
        ahi (hyperthreading) no acelera la inferencia y calienta de mas.
        """
        if self.cfg.cpu_threads > 0:
            return self.cfg.cpu_threads
        try:
            import os
            physical = os.cpu_count() or 4
            return max(1, physical // 2) if physical > 4 else physical
        except Exception:
            return 4

    def _resolve_device(self) -> tuple[str, str]:
        device = self.cfg.device
        if device == "auto":
            device = "cuda" if _cuda_available() else "cpu"

        compute = self.cfg.compute_type
        if compute == "auto":
            compute = "float16" if device == "cuda" else "int8"
        return device, compute

    # -- transcripcion --------------------------------------------------
    def transcribe(self, path: Path, duration: float) -> Transcript:
        cache_path = self.cache_dir / f"{self._cache_key(path)}.json"
        if cache_path.is_file():
            try:
                log.info("Transcripcion en cache: %s", cache_path.name)
                return _transcript_from_dict(json.loads(cache_path.read_text(encoding="utf-8")))
            except (OSError, json.JSONDecodeError, KeyError, TypeError):
                log.warning("Cache corrupta (%s); se vuelve a transcribir.", cache_path.name)

        log.info("Transcribiendo %s (%s)...", path.name, _fmt_duration(duration))
        started = time.perf_counter()

        raw_segments, info = self._run(str(path))

        segments: List[Segment] = []
        # faster-whisper devuelve un generador perezoso: el trabajo real ocurre
        # aqui, por eso mostramos el avance sobre la duracion total.
        for seg in raw_segments:
            segment = _to_segment(seg)
            if segment is None:
                continue
            segments.append(segment)
            _progress(segment.end, duration)

        _progress(duration, duration, final=True)
        language = info.language

        if not segments:
            raise RuntimeError(
                f"La transcripcion de '{path.name}' no devolvio ninguna palabra. "
                "Comprueba que el video tenga audio audible."
            )

        transcript = Transcript(
            language=language or (self.cfg.language or "??"),
            duration=duration,
            segments=segments,
        )
        elapsed = time.perf_counter() - started
        log.info("Transcritas %d palabras en %.0fs (%.1fx tiempo real). Idioma: %s",
                 len(transcript.words), elapsed,
                 duration / elapsed if elapsed else 0, transcript.language)

        cache_path.write_text(
            json.dumps(_transcript_to_dict(transcript), ensure_ascii=False),
            encoding="utf-8",
        )
        return transcript

    # -- transcripcion por zonas ----------------------------------------
    def transcribe_regions(self, path: Path, duration: float,
                           regions: Sequence[tuple[float, float]]) -> Transcript:
        """Transcribe solo los tramos indicados, con tiempos absolutos.

        El resultado se comporta como una transcripcion normal salvo que entre
        zona y zona no hay palabras. Es lo unico que necesita el pipeline: los
        huecos son justamente las partes del podcast que nadie va a recortar.
        """
        regions = [(float(a), float(b)) for a, b in regions if b - a > 0.5]
        if not regions:
            return self.transcribe(path, duration)

        cache_path = self.cache_dir / f"{self._cache_key(path, regions)}.json"
        if cache_path.is_file():
            try:
                log.info("Transcripcion en cache: %s", cache_path.name)
                return _transcript_from_dict(json.loads(cache_path.read_text(encoding="utf-8")))
            except (OSError, json.JSONDecodeError, KeyError, TypeError):
                log.warning("Cache corrupta (%s); se vuelve a transcribir.", cache_path.name)

        from .ffmpeg_utils import decode_audio_mono

        covered = sum(end - start for start, end in regions)
        log.info("Transcribiendo %d zonas de %s (%s en total, no %s)...",
                 len(regions), path.name, _fmt_duration(covered), _fmt_duration(duration))
        started = time.perf_counter()

        segments: List[Segment] = []
        language: Optional[str] = None
        done = 0.0
        for start, end in regions:
            # Decodificamos solo el tramo: ffmpeg busca en el fichero, no lee
            # las tres horas. Pasarle a Whisper el array ya decodificado evita
            # ademas que vuelva a abrir el video por su cuenta.
            audio = decode_audio_mono(path, sample_rate=16000,
                                      start=start, duration=end - start)
            raw_segments, info = self._run(audio)
            if language is None:
                language = info.language
            for seg in raw_segments:
                segment = _to_segment(seg, offset=start)
                if segment is None:
                    continue
                segments.append(segment)
                _progress(done + (segment.end - start), covered)
            done += end - start
            _progress(done, covered)

        _progress(covered, covered, final=True)

        if not segments:
            raise RuntimeError(
                f"La transcripcion de '{path.name}' no devolvio ninguna palabra. "
                "Comprueba que el video tenga audio audible."
            )

        segments.sort(key=lambda s: s.start)
        transcript = Transcript(
            language=language or (self.cfg.language or "??"),
            duration=duration,
            segments=segments,
        )
        elapsed = time.perf_counter() - started
        log.info("Transcritas %d palabras en %.0fs (%.1fx tiempo real sobre lo "
                 "transcrito, %.0fx sobre el video). Idioma: %s",
                 len(transcript.words), elapsed,
                 covered / elapsed if elapsed else 0,
                 duration / elapsed if elapsed else 0, transcript.language)

        cache_path.write_text(
            json.dumps(_transcript_to_dict(transcript), ensure_ascii=False),
            encoding="utf-8",
        )
        return transcript

    # -- opciones comunes -----------------------------------------------
    def _options(self) -> dict:
        return {
            "language": self.cfg.language,
            "beam_size": self.cfg.beam_size,
            "word_timestamps": True,
            "vad_filter": self.cfg.vad_filter,
            "condition_on_previous_text": self.cfg.condition_on_previous_text,
        }

    def _engine(self):
        """Motor de transcripcion: por lotes si la version lo trae.

        `BatchedInferencePipeline` (faster-whisper >= 1.1) trocea el audio por
        voz y transcribe varios trozos a la vez. Es 2-4x mas rapido con el
        mismo modelo y las mismas marcas de tiempo por palabra. Si la version
        instalada no lo tiene, se usa el modelo normal sin decir nada.
        """
        model = self._load_model()
        if not getattr(self.cfg, "batched", True):
            return model
        if self._batched is not None:
            return self._batched

        try:
            from faster_whisper import BatchedInferencePipeline
        except ImportError:
            log.info("La version de faster-whisper instalada no transcribe por "
                     "lotes; se usa el modo normal.")
            self._batched = model
            return model

        try:
            self._batched = BatchedInferencePipeline(model=model)
            log.info("Transcripcion por lotes activada (batch_size=%d).",
                     self.cfg.batch_size)
        except Exception as exc:
            log.warning("No se pudo activar la transcripcion por lotes (%s); "
                        "se usa el modo normal.", exc)
            self._batched = model
        return self._batched

    def _run(self, audio):
        """Lanza la transcripcion con el motor y las opciones que toquen."""
        engine = self._engine()
        options = self._options()
        if engine is not self._model:
            options["batch_size"] = max(1, int(self.cfg.batch_size))
        return engine.transcribe(audio, **options)

    def _cache_key(self, path: Path, regions: Optional[Sequence] = None) -> str:
        stat = path.stat()
        payload = "|".join(str(x) for x in (
            path.resolve(), stat.st_size, int(stat.st_mtime),
            self.cfg.model_size, self.cfg.language,
            self.cfg.beam_size, self.cfg.vad_filter,
            self.cfg.condition_on_previous_text,
            getattr(self.cfg, "batched", True),
            getattr(self.cfg, "batch_size", 8),
            # Zonas distintas = transcripcion distinta: no pueden compartir
            # entrada de cache con la del video entero.
            "|".join(f"{a:.1f}-{b:.1f}" for a, b in regions) if regions else "full",
        ))
        return hashlib.sha1(payload.encode("utf-8")).hexdigest()[:16]


# ----------------------------------------------------------------------------

def _to_segment(seg, offset: float = 0.0) -> Optional[Segment]:
    """Convierte un segmento de faster-whisper, desplazando los tiempos."""
    words = [
        Word(text=w.word, start=float(w.start) + offset, end=float(w.end) + offset,
             probability=float(getattr(w, "probability", 1.0)))
        for w in (seg.words or [])
        if w.start is not None and w.end is not None and w.word.strip()
    ]
    if not words:
        return None
    return Segment(
        start=float(seg.start) + offset,
        end=float(seg.end) + offset,
        text=seg.text.strip(),
        words=words,
    )


def _cuda_available() -> bool:
    try:
        import ctranslate2
        return ctranslate2.get_cuda_device_count() > 0
    except Exception:
        return False


def _progress(current: float, total: float, final: bool = False) -> None:
    if total <= 0:
        return
    progress.progress("transcribe", current / total, "transcribiendo", final=final)


def _fmt_duration(seconds: float) -> str:
    minutes, secs = divmod(int(seconds), 60)
    hours, minutes = divmod(minutes, 60)
    return f"{hours}h{minutes:02d}m{secs:02d}s" if hours else f"{minutes}m{secs:02d}s"


def _transcript_to_dict(t: Transcript) -> dict:
    return {
        "language": t.language,
        "duration": t.duration,
        "segments": [
            {"start": s.start, "end": s.end, "text": s.text,
             "words": [asdict(w) for w in s.words]}
            for s in t.segments
        ],
    }


def _transcript_from_dict(data: dict) -> Transcript:
    return Transcript(
        language=data["language"],
        duration=data["duration"],
        segments=[
            Segment(
                start=s["start"], end=s["end"], text=s["text"],
                words=[Word(**w) for w in s["words"]],
            )
            for s in data["segments"]
        ],
    )
