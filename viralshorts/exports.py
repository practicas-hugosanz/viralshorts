"""Hoja de calculo con los textos de publicacion de todos los shorts.

Subir diez shorts es copiar y pegar diez titulos, diez descripciones y diez
listas de hashtags desde diez ficheros distintos. Esto los junta en un CSV que
se abre en Excel o en Google Sheets y sirve como calendario de publicacion.

Se reconstruye a partir de los .json que el pipeline deja junto a cada video,
asi que incluye tambien los shorts de ejecuciones anteriores que sigan en la
carpeta.
"""

from __future__ import annotations

import csv
import json
import logging
from pathlib import Path
from typing import List, Optional

log = logging.getLogger(__name__)

_COLUMNS = ["fichero", "titulo", "gancho", "descripcion", "hashtags",
            "duracion", "minuto_original", "podcast", "url_origen", "puntuacion"]


def write_csv(output_dir: Path, name: str = "publicaciones.csv") -> Optional[Path]:
    """Escribe (o rehace) el CSV de la carpeta de salida."""
    output_dir = Path(output_dir)
    rows = _rows(output_dir)
    if not rows:
        return None

    path = output_dir / name
    try:
        # utf-8-sig: sin el BOM, Excel en Windows abre las tildes rotas.
        with path.open("w", encoding="utf-8-sig", newline="") as handle:
            writer = csv.DictWriter(handle, fieldnames=_COLUMNS, delimiter=";")
            writer.writeheader()
            writer.writerows(rows)
    except OSError as exc:
        log.warning("No se pudo escribir %s (%s).", name, exc)
        return None

    log.info("Textos de publicacion en %s (%d shorts).", path, len(rows))
    return path


def _rows(output_dir: Path) -> List[dict]:
    rows = []
    for video in sorted(output_dir.glob("short_*.mp4")):
        sidecar = video.with_suffix(".json")
        if not sidecar.is_file():
            continue
        try:
            data = json.loads(sidecar.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            continue

        publicacion = data.get("publicacion") or {}
        fuente = data.get("fuente") or {}
        rows.append({
            "fichero": video.name,
            "titulo": publicacion.get("title", ""),
            "gancho": publicacion.get("hook", ""),
            "descripcion": publicacion.get("description", ""),
            "hashtags": " ".join(f"#{t.lstrip('#')}"
                                 for t in publicacion.get("hashtags", [])),
            "duracion": data.get("duracion_final", ""),
            "minuto_original": fuente.get("marca", ""),
            "podcast": fuente.get("titulo", ""),
            "url_origen": fuente.get("url", "") or "",
            "puntuacion": (data.get("puntuacion") or {}).get("total", ""),
        })
    return rows
