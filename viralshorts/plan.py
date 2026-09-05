"""Plan de montaje: la seleccion de clips, guardada para revisarla.

Elegir los momentos cuesta minutos (descarga, transcripcion, Claude);
renderizarlos cuesta otros tantos. Antes las dos cosas iban pegadas, asi que
para cambiar un titulo o descartar un clip malo habia que repetirlo todo.

El plan parte el pipeline en dos. La primera mitad escribe aqui QUE se va a
montar -- con las palabras y sus tiempos, que es lo unico que el render
necesita de la transcripcion -- y la segunda mitad lo lee. En medio cabe una
persona: marcar los clips que valen, reescribir un gancho, cambiar el color de
los subtitulos, y montar solo eso.

El fichero lleva ademas la configuracion completa con la que se eligio, de modo
que el render reproduce exactamente los mismos ajustes aunque config.yaml haya
cambiado por el camino.
"""

from __future__ import annotations

import json
import logging
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Dict, List, Optional

from .llm import ClipMetadata
from .scoring import Window, hms
from .transcribe import Word

log = logging.getLogger(__name__)

FORMAT_VERSION = 2


@dataclass
class PlanClip:
    """Un candidato del plan, con todo lo que hace falta para montarlo."""
    start: float
    end: float
    score: float = 0.0
    reason: str = ""
    text: str = ""
    selected: bool = True
    signals: Dict[str, float] = field(default_factory=dict)
    meta: Optional[ClipMetadata] = None
    words: List[Word] = field(default_factory=list)

    @property
    def duration(self) -> float:
        return self.end - self.start

    def to_window(self) -> Window:
        """Reconstruye la ventana que espera el render."""
        window = Window(start=self.start, end=self.end,
                        words=list(self.words), text=self.text)
        window.score = self.score
        window.reason = self.reason
        for name in ("heat", "energy", "density", "context",
                     "llm_content", "llm_hook"):
            setattr(window, name, float(self.signals.get(name, 0.0)))
        return window


@dataclass
class Plan:
    path: Path
    config: dict
    podcast: dict
    background: dict
    clips: List[PlanClip]

    @property
    def selected(self) -> List[int]:
        return [i for i, clip in enumerate(self.clips) if clip.selected]


# ----------------------------------------------------------------------------
# Escritura
# ----------------------------------------------------------------------------

def save(path: Path, cfg, podcast, background, clips: List[Window],
         metadata: Dict[int, ClipMetadata], selected: int) -> Path:
    """Guarda la seleccion. `selected` = cuantos vienen marcados de serie."""
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)

    data = {
        "version": FORMAT_VERSION,
        "creado": time.strftime("%Y-%m-%d %H:%M:%S"),
        "config": cfg.to_dict(),
        "podcast": _source_dict(podcast),
        "background": _source_dict(background),
        "clips": [
            _clip_dict(clip, metadata.get(index), index < selected)
            for index, clip in enumerate(clips)
        ],
    }
    path.write_text(json.dumps(data, ensure_ascii=False, indent=1), encoding="utf-8")
    log.info("Plan guardado en %s (%d candidatos, %d marcados).",
             path, len(clips), min(selected, len(clips)))
    return path


def _source_dict(source) -> dict:
    info = getattr(source, "info", {}) or {}
    return {
        "ruta": str(Path(source.path).absolute()),
        "titulo": source.title,
        "duracion": round(float(source.duration), 2),
        "url": info.get("webpage_url", ""),
    }


def _clip_dict(clip: Window, meta: Optional[ClipMetadata], selected: bool) -> dict:
    return {
        "inicio": round(clip.start, 3),
        "fin": round(clip.end, 3),
        "marca": hms(clip.start),
        "duracion": round(clip.duration, 1),
        "puntuacion": round(clip.score, 4),
        "motivo": clip.reason,
        "texto": clip.text,
        "seleccionado": bool(selected),
        "senales": {
            "heat": round(clip.heat, 4),
            "energy": round(clip.energy, 4),
            "density": round(clip.density, 4),
            "context": round(clip.context, 4),
            "llm_content": round(clip.llm_content, 4),
            "llm_hook": round(clip.llm_hook, 4),
        },
        "publicacion": ({
            "title": meta.title, "hook": meta.hook,
            "description": meta.description, "hashtags": list(meta.hashtags),
        } if meta else None),
        # Compacto a proposito: son miles de palabras y un diccionario por
        # palabra multiplica por cinco el tamano del fichero sin aportar nada.
        "palabras": [[w.text, round(w.start, 3), round(w.end, 3),
                      round(w.probability, 3)] for w in clip.words],
    }


# ----------------------------------------------------------------------------
# Lectura
# ----------------------------------------------------------------------------

def load(path: Path) -> Plan:
    path = Path(path)
    if not path.is_file():
        raise FileNotFoundError(f"No existe el plan: {path}")

    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except json.JSONDecodeError as exc:
        raise ValueError(f"El plan {path.name} esta corrupto: {exc}") from exc

    if int(data.get("version", 0)) != FORMAT_VERSION:
        raise ValueError(
            f"El plan {path.name} es de otra version de la aplicacion. "
            "Vuelve a buscar los momentos para generar uno nuevo."
        )

    return Plan(
        path=path,
        config=data.get("config") or {},
        podcast=data.get("podcast") or {},
        background=data.get("background") or {},
        clips=[_clip_from_dict(c) for c in data.get("clips", [])],
    )


def _clip_from_dict(data: dict) -> PlanClip:
    publicacion = data.get("publicacion") or None
    return PlanClip(
        start=float(data["inicio"]),
        end=float(data["fin"]),
        score=float(data.get("puntuacion", 0.0)),
        reason=str(data.get("motivo", "")),
        text=str(data.get("texto", "")),
        selected=bool(data.get("seleccionado", True)),
        signals={k: float(v) for k, v in (data.get("senales") or {}).items()},
        meta=(ClipMetadata(
            title=str(publicacion.get("title", "")),
            hook=str(publicacion.get("hook", "")),
            description=str(publicacion.get("description", "")),
            hashtags=[str(t) for t in publicacion.get("hashtags", [])],
        ) if publicacion else None),
        words=[Word(text=w[0], start=float(w[1]), end=float(w[2]),
                    probability=float(w[3]) if len(w) > 3 else 1.0)
               for w in data.get("palabras", [])],
    )


def update(path: Path, changes: List[dict]) -> Plan:
    """Aplica los cambios de la interfaz (marcar, titular) sobre el fichero.

    `changes` trae una entrada por clip con las claves que se hayan tocado:
    `seleccionado`, `publicacion` (title/hook/description/hashtags) y, si el
    usuario ha cambiado el estilo, `config` con los ajustes nuevos.
    """
    path = Path(path)
    data = json.loads(path.read_text(encoding="utf-8"))
    clips = data.get("clips", [])

    for change in changes:
        index = int(change.get("n", 0)) - 1
        if not 0 <= index < len(clips):
            continue
        if "seleccionado" in change:
            clips[index]["seleccionado"] = bool(change["seleccionado"])
        publicacion = change.get("publicacion")
        if publicacion:
            actual = clips[index].get("publicacion") or {
                "title": "", "hook": "", "description": "", "hashtags": []}
            for key in ("title", "hook", "description"):
                if key in publicacion:
                    actual[key] = str(publicacion[key]).strip()
            if "hashtags" in publicacion:
                actual["hashtags"] = [str(t).lstrip("#").strip()
                                      for t in publicacion["hashtags"]
                                      if str(t).strip()]
            clips[index]["publicacion"] = actual

    path.write_text(json.dumps(data, ensure_ascii=False, indent=1), encoding="utf-8")
    return load(path)


def apply_config(path: Path, overrides: Dict[str, object]) -> None:
    """Cambia ajustes del plan ('subtitles.font_size': 80) antes de renderizar."""
    path = Path(path)
    data = json.loads(path.read_text(encoding="utf-8"))
    config = data.setdefault("config", {})

    for dotted, value in overrides.items():
        target = config
        parts = dotted.split(".")
        for part in parts[:-1]:
            target = target.setdefault(part, {})
            if not isinstance(target, dict):
                raise ValueError(f"'{dotted}' no es un ajuste valido.")
        target[parts[-1]] = value

    path.write_text(json.dumps(data, ensure_ascii=False, indent=1), encoding="utf-8")
