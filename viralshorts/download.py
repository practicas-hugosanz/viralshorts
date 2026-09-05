"""Descarga de videos con yt-dlp, con cache y extraccion del heatmap.

Ademas del fichero de video guardamos el `info.json` de yt-dlp porque contiene
el campo `heatmap` ("most replayed" de YouTube): la senal mas barata y mas
fiable que existe para saber que parte de un video engancha, porque la ha
generado la propia audiencia rebobinando.
"""

from __future__ import annotations

import json
import logging
import time
from dataclasses import dataclass
from pathlib import Path
from typing import List, Optional

from . import progress

log = logging.getLogger(__name__)

_VIDEO_EXTS = (".mp4", ".mkv", ".webm", ".mov", ".m4v", ".avi")


@dataclass
class HeatPoint:
    start: float
    end: float
    value: float          # 0..1, relativo al resto del video


@dataclass
class SourceVideo:
    path: Path
    title: str
    duration: float
    heatmap: List[HeatPoint]
    info: dict

    @property
    def has_heatmap(self) -> bool:
        return len(self.heatmap) > 1


def fetch(url: str, dest_base: Path, cfg) -> SourceVideo:
    """Devuelve un `SourceVideo` para una URL o una ruta local.

    Si `url` apunta a un fichero que ya existe en disco, se usa tal cual (sin
    descargar nada). Si ya hay una descarga previa con el mismo `dest_base`,
    se reutiliza en vez de volver a bajar el video.
    """
    local = Path(url)
    if local.is_file():
        log.info("Usando fichero local: %s", local)
        return _from_local_file(local)

    dest_base.parent.mkdir(parents=True, exist_ok=True)
    cached = _find_existing(dest_base)
    info_path = dest_base.with_suffix(".info.json")

    if cached:
        log.info("Reutilizando descarga previa: %s", cached.name)
        return _build(cached, _cached_info(url, info_path, cfg))

    info = _download(url, dest_base, cfg)
    downloaded = _find_existing(dest_base)
    if downloaded is None:
        raise RuntimeError(
            f"yt-dlp dijo que descargo '{url}' pero no hay ningun fichero en "
            f"{dest_base.parent} que empiece por '{dest_base.name}'."
        )

    _save_info(info, info_path)
    return _build(downloaded, info)


def _cached_info(url: str, info_path: Path, cfg) -> dict:
    """Metadatos de una descarga previa, recuperandolos si hiciera falta."""
    if info_path.is_file():
        try:
            return json.loads(info_path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            log.warning("%s ilegible; se vuelven a pedir los metadatos.", info_path.name)

    # El video ya esta en disco pero no sus metadatos. Los pedimos SIN volver a
    # descargar el video: bajar otra vez varios GB por un heatmap no compensa.
    log.info("Recuperando metadatos (sin volver a descargar el video)...")
    try:
        info = _extract(url, cfg, download=False)
    except Exception as exc:
        log.warning("No se pudieron recuperar los metadatos (%s). "
                    "Se sigue sin heatmap.", exc)
        return {}

    _save_info(info, info_path)
    return info


def _save_info(info: dict, info_path: Path) -> None:
    """Guarda los metadatos que usamos. Nunca aborta el pipeline."""
    try:
        info_path.write_text(
            json.dumps(info, ensure_ascii=False, default=str), encoding="utf-8"
        )
    except (OSError, TypeError, ValueError) as exc:
        log.warning("No se pudo guardar %s (%s); no es critico.",
                    info_path.name, exc)


# ----------------------------------------------------------------------------

def _download(url: str, dest_base: Path, cfg) -> dict:
    log.info("Descargando %s", url)
    return _extract(url, cfg, download=True, dest_base=dest_base)


def _format_string(cfg) -> str:
    """El selector de formato de yt-dlp, con la altura maxima puesta.

    El valor por defecto lleva `{max_height}`; un selector escrito a mano en
    config.yaml se usa tal cual (aunque lleve llaves raras, de ahi el try).
    """
    template = cfg.format
    try:
        return template.format(max_height=int(getattr(cfg, "max_height", 1080)))
    except (KeyError, IndexError, ValueError):
        return template


def _extract(url: str, cfg, download: bool, dest_base: Optional[Path] = None) -> dict:
    """Llama a yt-dlp y devuelve unos metadatos limpios y serializables."""
    try:
        import yt_dlp
    except ImportError as exc:
        raise ImportError("Falta yt-dlp. Ejecuta: pip install yt-dlp") from exc

    options = {
        "format": _format_string(cfg),
        "merge_output_format": "mp4",
        "retries": cfg.retries,
        # La barra de yt-dlp usa retorno de carro: en un log sin terminal (la
        # interfaz, o una redireccion a fichero) se convierte en una sola linea
        # de cientos de KB. Usamos la nuestra a traves de progress_hooks.
        "noprogress": True,
        "quiet": True,
        "no_warnings": True,
        "noplaylist": True,
        "progress_hooks": [_report_download],
        # YouTube sirve los videos largos en cientos de fragmentos y yt-dlp los
        # pide de uno en uno: la descarga tarda mucho mas que la propia
        # conexion. Bajarlos en paralelo es la mejora mas barata del pipeline.
        "concurrent_fragment_downloads": max(1, int(getattr(cfg, "concurrent_fragments", 8))),
        "http_chunk_size": max(1 << 20, int(getattr(cfg, "http_chunk_size", 10_485_760))),
    }
    if dest_base is not None:
        options["outtmpl"] = f"{dest_base}.%(ext)s"
    if cfg.cookies_from_browser:
        options["cookiesfrombrowser"] = (cfg.cookies_from_browser,)
    if cfg.cookiefile:
        options["cookiefile"] = cfg.cookiefile

    with yt_dlp.YoutubeDL(options) as ydl:
        info = ydl.extract_info(url, download=download)
        # El info_dict crudo trae objetos internos de yt-dlp (postprocesadores,
        # etc.) que no son serializables a JSON. sanitize_info los quita.
        info = ydl.sanitize_info(info)

    # Con `noplaylist` deberia venir un solo video, pero algunas URLs (mixes,
    # canales, directos) siguen devolviendo una lista; cogemos la primera.
    if info.get("_type") == "playlist":
        entries = [e for e in (info.get("entries") or []) if e]
        if not entries:
            raise RuntimeError(f"'{url}' no devolvio ningun video.")
        info = entries[0]

    return _slim(info)


# De todo lo que devuelve yt-dlp (cientos de campos, incluida la lista completa
# de formatos) solo usamos esto. Guardar el resto engorda el fichero y es una
# fuente constante de problemas de serializacion.
_KEEP = ("id", "title", "duration", "webpage_url", "original_url", "uploader",
         "channel", "upload_date", "heatmap", "chapters", "language",
         "view_count", "like_count", "is_live", "was_live")


def _slim(info: dict) -> dict:
    return {key: info[key] for key in _KEEP if info.get(key) is not None}


_LAST_REPORT = 0.0


def _report_download(status: dict) -> None:
    """Hook de yt-dlp: traduce su avance a nuestra barra de progreso."""
    global _LAST_REPORT

    if status.get("status") == "finished":
        progress.progress("descarga", 1.0, "descargando", final=True)
        _LAST_REPORT = 0.0
        return
    if status.get("status") != "downloading":
        return

    total = status.get("total_bytes") or status.get("total_bytes_estimate")
    done = status.get("downloaded_bytes")
    if not total or not done:
        return

    # El hook salta muchas veces por segundo; sin freno llenariamos el log.
    now = time.monotonic()
    if now - _LAST_REPORT < 0.5:
        return
    _LAST_REPORT = now

    megabytes = done / 1e6, total / 1e6
    progress.progress("descarga", done / total,
                      f"descargando {megabytes[0]:.0f}/{megabytes[1]:.0f} MB")


def _find_existing(dest_base: Path) -> Optional[Path]:
    """Busca un fichero ya descargado, priorizando mp4."""
    candidates = [
        p for p in dest_base.parent.glob(f"{dest_base.name}.*")
        if p.suffix.lower() in _VIDEO_EXTS and p.stat().st_size > 0
    ]
    if not candidates:
        return None
    candidates.sort(key=lambda p: (p.suffix.lower() != ".mp4", -p.stat().st_size))
    return candidates[0]


def _build(path: Path, info: dict) -> SourceVideo:
    from .ffmpeg_utils import probe

    duration = float(info.get("duration") or 0) or probe(path).duration
    heatmap = _parse_heatmap(info)
    if heatmap:
        log.info("Heatmap de YouTube disponible (%d puntos): se usara para puntuar.",
                 len(heatmap))
    else:
        log.info("Este video no expone heatmap; se puntuara sin esa senal.")

    return SourceVideo(
        path=path,
        title=info.get("title") or path.stem,
        duration=duration,
        heatmap=heatmap,
        info=info,
    )


def _from_local_file(path: Path) -> SourceVideo:
    from .ffmpeg_utils import probe

    # Un fichero local puede tener al lado el info.json de una descarga previa.
    info_path = path.with_suffix(".info.json")
    info = {}
    if info_path.is_file():
        try:
            info = json.loads(info_path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            log.warning("No se pudo leer %s; se ignora.", info_path.name)

    return SourceVideo(
        path=path,
        title=info.get("title") or path.stem,
        duration=probe(path).duration,
        heatmap=_parse_heatmap(info),
        info=info,
    )


def _parse_heatmap(info: dict) -> List[HeatPoint]:
    raw = info.get("heatmap")
    if not isinstance(raw, list):
        return []

    points: List[HeatPoint] = []
    for item in raw:
        if not isinstance(item, dict):
            continue
        try:
            points.append(HeatPoint(
                start=float(item["start_time"]),
                end=float(item["end_time"]),
                value=float(item["value"]),
            ))
        except (KeyError, TypeError, ValueError):
            continue

    points.sort(key=lambda p: p.start)
    return points
