"""Canal de progreso, con dos formatos segun quien este mirando.

En una terminal se pinta una barra con retorno de carro. Cuando el pipeline lo
lanza la interfaz grafica (variable de entorno VIRALSHORTS_PROGRESS=json) se
emiten lineas JSON con el prefijo @@VS, que la GUI parsea para mover sus barras.
Sin ninguna de las dos cosas (por ejemplo redirigiendo a un fichero) no se
escribe nada, para no llenar el log de basura.
"""

from __future__ import annotations

import json
import os
import sys
from typing import Optional

PREFIX = "@@VS "

_MODE: Optional[str] = None
_BAR_WIDTH = 28


def mode() -> str:
    """'tty', 'json' u 'off'. Se resuelve una vez y se recuerda."""
    global _MODE
    if _MODE is None:
        requested = os.environ.get("VIRALSHORTS_PROGRESS", "auto").lower()
        if requested == "json":
            _MODE = "json"
        elif requested == "off":
            _MODE = "off"
        elif requested == "tty" or sys.stderr.isatty():
            _MODE = "tty"
        else:
            _MODE = "off"
    return _MODE


def stage(name: str, label: str = "") -> None:
    """Marca el comienzo de una fase del pipeline."""
    if mode() == "json":
        _write({"type": "stage", "stage": name, "label": label})


_SUPPRESSED: set = set()


def suppress(stage_name: str) -> None:
    """Silencia el avance de una fase.

    Con varios renders a la vez, cada ffmpeg reporta su propio porcentaje y la
    barra salta hacia atras cada dos segundos. En ese caso se silencia 'render'
    y quien coordina informa del avance global.
    """
    _SUPPRESSED.add(stage_name)


def allow(stage_name: str) -> None:
    _SUPPRESSED.discard(stage_name)


def progress(stage_name: str, fraction: float, label: str = "",
             final: bool = False) -> None:
    """Avance dentro de una fase. `fraction` va de 0 a 1."""
    current = mode()
    if current == "off" or stage_name in _SUPPRESSED:
        return

    fraction = min(max(fraction, 0.0), 1.0)
    if current == "json":
        _write({"type": "progress", "stage": stage_name,
                "pct": round(fraction * 100, 1), "label": label})
        return

    filled = int(fraction * _BAR_WIDTH)
    bar = "#" * filled + "-" * (_BAR_WIDTH - filled)
    sys.stderr.write(f"\r  {label or stage_name} [{bar}] {fraction * 100:5.1f}%")
    if final:
        sys.stderr.write("\n")
    sys.stderr.flush()


def result(kind: str, **data) -> None:
    """Evento estructurado para la GUI (clip elegido, fichero generado...)."""
    if mode() == "json":
        _write({"type": kind, **data})


def _write(payload: dict) -> None:
    sys.stderr.write(PREFIX + json.dumps(payload, ensure_ascii=False) + "\n")
    sys.stderr.flush()
