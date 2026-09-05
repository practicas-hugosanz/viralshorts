"""Envoltorio fino sobre ffmpeg/ffprobe.

Todo el trabajo de video del pipeline pasa por aqui. Deliberadamente NO usamos
moviepy: una sola llamada a ffmpeg con un filter_complex evita recodificar el
video varias veces (moviepy re-renderiza y luego habria que volver a pasar por
ffmpeg para quemar los subtitulos, con doble perdida de calidad).
"""

from __future__ import annotations

import json
import logging
import shutil
import subprocess
from dataclasses import dataclass
from pathlib import Path
from typing import List, Optional, Sequence

from . import progress

log = logging.getLogger(__name__)


class FFmpegError(RuntimeError):
    """ffmpeg o ffprobe terminaron con error."""


@dataclass
class MediaInfo:
    path: str
    width: int
    height: int
    duration: float
    fps: float
    has_audio: bool
    # Proporcion del pixel. Casi siempre 1, pero hay grabaciones (DV, algunas
    # emisiones, video anamorfico) con pixeles no cuadrados: ahi width/height
    # NO es la proporcion con la que se ve el video, y usarla para calcular
    # recortes deforma la imagen.
    sar: float = 1.0

    @property
    def display_width(self) -> int:
        """Anchura con la que el video se ve realmente, en pixeles cuadrados."""
        return max(2, _even(int(round(self.width * self.sar))))

    @property
    def square_pixels(self) -> bool:
        return abs(self.sar - 1.0) < 1e-3

    @property
    def aspect(self) -> float:
        return self.display_width / self.height if self.height else 1.0


def _even(value: int) -> int:
    """Los codecs 4:2:0 exigen dimensiones pares."""
    return value - (value % 2)


# ----------------------------------------------------------------------------
# Comprobaciones y sondeo
# ----------------------------------------------------------------------------

def require_ffmpeg() -> None:
    """Falla pronto y con un mensaje util si falta el binario."""
    missing = [b for b in ("ffmpeg", "ffprobe") if shutil.which(b) is None]
    if missing:
        raise FFmpegError(
            f"No se encontro {' ni '.join(missing)} en el PATH.\n"
            "  Windows: winget install Gyan.FFmpeg   (o choco install ffmpeg)\n"
            "  macOS:   brew install ffmpeg\n"
            "  Linux:   sudo apt install ffmpeg\n"
            "Comprueba luego con: ffmpeg -version"
        )


def probe(path: str | Path) -> MediaInfo:
    """Lee dimensiones, duracion, fps y presencia de audio de un fichero."""
    path = str(path)
    cmd = [
        "ffprobe", "-v", "error",
        "-print_format", "json",
        "-show_format", "-show_streams",
        path,
    ]
    result = subprocess.run(cmd, capture_output=True, text=True, encoding="utf-8", errors="replace")
    if result.returncode != 0:
        raise FFmpegError(f"ffprobe fallo con '{path}':\n{result.stderr.strip()}")

    data = json.loads(result.stdout)
    streams = data.get("streams", [])
    video = next((s for s in streams if s.get("codec_type") == "video"), None)
    if video is None:
        raise FFmpegError(f"'{path}' no contiene ninguna pista de video.")

    has_audio = any(s.get("codec_type") == "audio" for s in streams)

    duration = _first_float(
        data.get("format", {}).get("duration"),
        video.get("duration"),
    )
    if duration is None:
        raise FFmpegError(f"No se pudo determinar la duracion de '{path}'.")

    width, height = int(video["width"]), int(video["height"])
    # Algunos moviles graban en vertical con la rotacion en metadatos; ffprobe
    # da width/height del stream sin rotar, asi que los intercambiamos.
    rotation = _rotation_of(video)
    sar = _parse_sar(video)
    if rotation in (90, 270):
        width, height = height, width
        sar = 1.0 / sar if sar else 1.0

    return MediaInfo(
        path=path,
        width=width,
        height=height,
        duration=duration,
        fps=_parse_fps(video),
        has_audio=has_audio,
        sar=sar,
    )


def _parse_sar(stream: dict) -> float:
    """Proporcion del pixel ('1:1', '4:3'...). 1.0 si no se declara."""
    raw = stream.get("sample_aspect_ratio") or "1:1"
    try:
        num, den = (int(x) for x in str(raw).split(":"))
    except (ValueError, TypeError):
        return 1.0
    if num <= 0 or den <= 0:
        return 1.0
    return num / den


def _first_float(*values) -> Optional[float]:
    for v in values:
        try:
            f = float(v)
        except (TypeError, ValueError):
            continue
        if f > 0:
            return f
    return None


def _rotation_of(stream: dict) -> int:
    tags = stream.get("tags") or {}
    try:
        rot = int(float(tags.get("rotate", 0)))
    except (TypeError, ValueError):
        rot = 0
    for sd in stream.get("side_data_list") or []:
        if "rotation" in sd:
            try:
                rot = int(float(sd["rotation"]))
            except (TypeError, ValueError):
                pass
    return abs(rot) % 360


def _parse_fps(stream: dict) -> float:
    for key in ("avg_frame_rate", "r_frame_rate"):
        raw = stream.get(key)
        if not raw or raw == "0/0":
            continue
        try:
            num, den = raw.split("/")
            value = float(num) / float(den)
        except (ValueError, ZeroDivisionError):
            continue
        if value > 0:
            return value
    return 30.0


# ----------------------------------------------------------------------------
# Ejecucion
# ----------------------------------------------------------------------------

def run(
    args: Sequence[str],
    *,
    cwd: str | Path | None = None,
    total_duration: float | None = None,
    desc: str = "ffmpeg",
) -> None:
    """Ejecuta ffmpeg mostrando una barra de progreso.

    `total_duration` es la duracion esperada de la SALIDA; se usa para calcular
    el porcentaje a partir de `-progress`.
    """
    cmd = ["ffmpeg", "-hide_banner", "-loglevel", "error", "-nostdin", "-y",
           "-progress", "pipe:1", "-nostats", *args]
    log.debug("Ejecutando: %s", " ".join(cmd))

    proc = subprocess.Popen(
        cmd,
        cwd=str(cwd) if cwd else None,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        encoding="utf-8",
        errors="replace",
        bufsize=1,
    )

    show_progress = bool(total_duration) and progress.mode() != "off"
    assert proc.stdout is not None
    try:
        for line in proc.stdout:
            if not show_progress or not line.startswith("out_time_us="):
                continue
            try:
                seconds = int(line.split("=", 1)[1]) / 1_000_000
            except ValueError:
                continue
            progress.progress("render", seconds / total_duration, desc)  # type: ignore[operator]
    finally:
        proc.stdout.close()
        stderr = proc.stderr.read() if proc.stderr else ""
        if proc.stderr:
            proc.stderr.close()
        code = proc.wait()
        if show_progress:
            progress.progress("render", 1.0, desc, final=True)

    if code != 0:
        raise FFmpegError(
            f"ffmpeg fallo (codigo {code}).\n"
            f"--- comando ---\n{' '.join(cmd)}\n"
            f"--- stderr ---\n{stderr.strip()[-4000:]}"
        )
    if stderr.strip():
        log.debug("ffmpeg stderr: %s", stderr.strip()[-2000:])


def decode_audio_mono(
    path: str | Path,
    sample_rate: int = 16000,
    start: float | None = None,
    duration: float | None = None,
):
    """Decodifica el audio a un array numpy mono float32 en [-1, 1]."""
    import numpy as np

    args = ["ffmpeg", "-v", "error", "-nostdin"]
    if start is not None:
        args += ["-ss", f"{start:.3f}"]
    args += ["-i", str(path)]
    if duration is not None:
        args += ["-t", f"{duration:.3f}"]
    args += ["-vn", "-ac", "1", "-ar", str(sample_rate), "-f", "s16le", "-"]

    result = subprocess.run(args, capture_output=True)
    if result.returncode != 0:
        raise FFmpegError(
            f"No se pudo decodificar el audio de '{path}':\n"
            f"{result.stderr.decode('utf-8', 'replace').strip()[-2000:]}"
        )

    raw = np.frombuffer(result.stdout, dtype="<i2")
    return raw.astype("float32") / 32768.0


_FILTER_OPTIONS_CACHE: dict[str, set[str]] = {}


def filter_supports(filter_name: str, option: str) -> bool:
    """Indica si el filtro acepta esa opcion en la version instalada.

    Las opciones de los filtros cambian entre versiones: `crop` tenia `eval`
    hasta ffmpeg 7 y en ffmpeg 8 lo elimino porque x/y ya se evaluan por
    fotograma. Preguntarselo al binario es mas fiable que parsear su version.
    """
    if filter_name not in _FILTER_OPTIONS_CACHE:
        result = subprocess.run(
            ["ffmpeg", "-hide_banner", "-h", f"filter={filter_name}"],
            capture_output=True, text=True, encoding="utf-8", errors="replace",
        )
        options = set()
        for line in result.stdout.splitlines():
            stripped = line.strip()
            # Las lineas de opciones tienen la forma "  nombre  <tipo>  ...".
            if stripped and line.startswith("   ") and "<" in stripped:
                options.add(stripped.split()[0])
        _FILTER_OPTIONS_CACHE[filter_name] = options

    return option in _FILTER_OPTIONS_CACHE[filter_name]


def escape_filter_path(path: str | Path) -> str:
    """Escapa una ruta para usarla dentro de un filtergraph de ffmpeg."""
    text = str(Path(path).absolute()).replace("\\", "/")
    return text.replace(":", r"\:").replace("'", r"\'")


def write_filter_script(graph: str, path: str | Path) -> List[str]:
    """Guarda el filtergraph en fichero y devuelve los argumentos de ffmpeg.

    Pasar el grafo por fichero evita el limite de longitud de la linea de
    comandos de Windows (~32k) y todos los problemas de escapado del shell,
    que es facil que aparezcan porque generamos expresiones muy largas.

    La ruta se devuelve ABSOLUTA a proposito: ffmpeg se lanza con cwd en la
    carpeta de trabajo (para que el filtro subtitles reciba un nombre corto),
    y una ruta relativa se resolveria dos veces contra esa misma carpeta.
    """
    Path(path).write_text(graph, encoding="utf-8")
    return ["-filter_complex_script", str(Path(path).absolute())]


# ----------------------------------------------------------------------------
# Codificadores por hardware
# ----------------------------------------------------------------------------

_ENCODERS_CACHE: Optional[set] = None
_WORKING_ENCODER_CACHE: dict[str, bool] = {}

# Orden de preferencia. NVENC es el mas rapido y el mas comun en estos equipos;
# QSV (Intel) y AMF (AMD) van despues. El ultimo recurso siempre es libx264.
_HW_CANDIDATES = ("h264_nvenc", "h264_qsv", "h264_amf")


def available_encoders() -> set:
    """Nombres de codificadores que declara el binario de ffmpeg instalado."""
    global _ENCODERS_CACHE
    if _ENCODERS_CACHE is None:
        try:
            result = subprocess.run(
                ["ffmpeg", "-hide_banner", "-encoders"],
                capture_output=True, text=True, encoding="utf-8", errors="replace",
                timeout=30,
            )
            names = set()
            for line in result.stdout.splitlines():
                parts = line.split()
                # Formato: " V....D h264_nvenc   NVIDIA NVENC H.264 encoder"
                if len(parts) >= 2 and parts[0].startswith("V"):
                    names.add(parts[1])
            _ENCODERS_CACHE = names
        except (OSError, subprocess.SubprocessError):
            _ENCODERS_CACHE = set()
    return _ENCODERS_CACHE


def encoder_works(name: str) -> bool:
    """Codifica un fotograma de prueba: declararlo no es lo mismo que tenerlo.

    ffmpeg lista h264_nvenc aunque no haya ninguna GPU NVIDIA en la maquina.
    Sin esta comprobacion el fallo aparece a mitad del render, despues de haber
    transcrito el podcast entero.
    """
    if name in _WORKING_ENCODER_CACHE:
        return _WORKING_ENCODER_CACHE[name]

    ok = False
    if name in available_encoders():
        try:
            result = subprocess.run(
                ["ffmpeg", "-hide_banner", "-loglevel", "error", "-nostdin", "-y",
                 "-f", "lavfi", "-i", "color=c=black:s=256x256:d=0.1",
                 "-frames:v", "1", "-c:v", name, "-f", "null", "-"],
                capture_output=True, text=True, encoding="utf-8", errors="replace",
                timeout=60,
            )
            ok = result.returncode == 0
        except (OSError, subprocess.SubprocessError):
            ok = False

    _WORKING_ENCODER_CACHE[name] = ok
    return ok


def detect_hardware_encoder(preference: str = "auto") -> Optional[str]:
    """Codificador por hardware utilizable, o None para usar libx264.

    `preference`: "auto" prueba NVENC, QSV y AMF por ese orden; "off" no prueba
    nada; cualquier otro valor se toma como el nombre exacto del codificador.
    """
    preference = (preference or "auto").lower()
    if preference in ("off", "no", "false", "none", "libx264", "cpu"):
        return None

    candidates = _HW_CANDIDATES if preference == "auto" else (preference,)
    for name in candidates:
        if encoder_works(name):
            log.info("Codificador por hardware disponible: %s.", name)
            return name

    if preference != "auto":
        log.warning("El codificador '%s' no funciona en esta maquina; "
                    "se usara libx264.", preference)
    return None
