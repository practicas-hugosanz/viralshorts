"""Render final: UNA sola pasada de ffmpeg.

El script original renderizaba con moviepy y despues volvia a pasar por ffmpeg
para quemar los subtitulos: dos codificaciones con perdida y el doble de
tiempo. Aqui se construye un unico filter_complex que hace todo de golpe:

    recorte -> jump cuts -> reencuadre con seguimiento de cara -> punch-in
    -> apilado vertical -> subtitulos -> normalizacion de volumen

El grafo se escribe en un fichero y se pasa con -filter_complex_script para no
chocar con el limite de longitud de la linea de comandos de Windows ni con el
escapado del shell, porque las expresiones de reencuadre son muy largas.
"""

from __future__ import annotations

import logging
from dataclasses import dataclass
from pathlib import Path
from typing import List, Optional, Sequence, Tuple

from .ffmpeg_utils import (FFmpegError, MediaInfo, filter_supports, probe, run,
                           write_filter_script)
from .facecrop import Track
from .jumpcuts import JumpCutPlan

log = logging.getLogger(__name__)


@dataclass
class RenderRequest:
    podcast: Path
    background: Path
    start: float
    end: float
    background_offset: float
    plan: JumpCutPlan
    track: Optional[Track]
    ass_name: Optional[str]        # nombre del .ass DENTRO de work_dir
    output: Path
    work_dir: Path


def render(request: RenderRequest, cfg) -> Path:
    """Genera el short vertical final. Devuelve la ruta del fichero."""
    podcast_info = probe(request.podcast)
    background_info = probe(request.background)

    layout = cfg.layout
    top_height = _even(int(layout.height * layout.top_ratio))
    bottom_height = layout.height - top_height
    duration = request.plan.output_duration

    graph = _build_graph(
        request=request,
        podcast_info=podcast_info,
        background_info=background_info,
        top_height=top_height,
        bottom_height=bottom_height,
        cfg=cfg,
    )

    request.work_dir.mkdir(parents=True, exist_ok=True)
    script_path = request.work_dir / f"{request.output.stem}.filter"
    filter_args = write_filter_script(graph, script_path)

    args: List[str] = [
        # Podcast: buscamos y recortamos en la entrada, mucho mas rapido que
        # decodificar el podcast entero para quedarnos con 40 segundos.
        "-ss", f"{request.start:.3f}",
        "-t", f"{request.end - request.start:.3f}",
        "-i", str(request.podcast.absolute()),
        # Fondo en bucle infinito: asi da igual que el gameplay sea mas corto
        # que la suma de todos los clips (el bug del script original).
        "-stream_loop", "-1",
        "-ss", f"{request.background_offset:.3f}",
        "-i", str(request.background.absolute()),
        *filter_args,
        "-map", "[vout]",
        "-map", "[aout]",
        *_video_encoder_args(cfg.output),
        "-pix_fmt", "yuv420p",
        "-r", str(layout.fps),
        "-c:a", "aac",
        "-b:a", cfg.output.audio_bitrate,
        "-ar", "48000",
        "-ac", "2",
        "-movflags", "+faststart",
        "-t", f"{duration:.3f}",
        "-shortest",
        str(request.output.absolute()),
    ]

    request.output.parent.mkdir(parents=True, exist_ok=True)
    # cwd = work_dir para que el filtro subtitles reciba un nombre relativo sin
    # letra de unidad ni dos puntos, que son un infierno de escapar en Windows.
    run(args, cwd=request.work_dir, total_duration=duration,
        desc=f"render {request.output.name}")

    log.info("Listo: %s (%.1fs)", request.output, duration)
    return request.output


# ----------------------------------------------------------------------------
# Construccion del filtergraph
# ----------------------------------------------------------------------------

def _build_graph(
    request: RenderRequest,
    podcast_info: MediaInfo,
    background_info: MediaInfo,
    top_height: int,
    bottom_height: int,
    cfg,
) -> str:
    fps = cfg.layout.fps
    width = cfg.layout.width
    chains: List[str] = []

    # --- panel superior: el podcast -------------------------------------
    top = ["[0:v]setpts=PTS-STARTPTS", f"fps={fps}", *_square_pixels(podcast_info)]
    if request.plan.enabled:
        top.append(f"select='{_select_expr(request.plan.keeps)}'")
        top.append("setpts=N/FRAME_RATE/TB")
    top.extend(_reframe_filters(
        source=podcast_info,
        target_width=width,
        target_height=top_height,
        track=request.track,
        plan=request.plan,
        cfg=cfg,
    ))
    top.append("setsar=1")
    chains.append(",".join(top) + "[top]")

    # --- panel inferior: el gameplay ------------------------------------
    bottom = [
        "[1:v]setpts=PTS-STARTPTS",
        f"fps={fps}",
        *_square_pixels(background_info),
        _center_crop(background_info, width, bottom_height),
        *_cover(width, bottom_height),
        "setsar=1",
    ]
    chains.append(",".join(bottom) + "[bottom]")

    # --- apilado + subtitulos -------------------------------------------
    stack = "[top][bottom]vstack=inputs=2"
    if request.ass_name:
        subtitles = f"subtitles=filename={request.ass_name}"
        if cfg.subtitles.fonts_dir:
            from .ffmpeg_utils import escape_filter_path
            subtitles += f":fontsdir={escape_filter_path(cfg.subtitles.fonts_dir)}"
        chains.append(f"{stack},{subtitles},format=yuv420p[vout]")
    else:
        chains.append(f"{stack},format=yuv420p[vout]")

    # --- audio: solo el del podcast, normalizado ------------------------
    audio = ["[0:a]asetpts=PTS-STARTPTS"]
    if request.plan.enabled:
        audio.append(f"aselect='{_select_expr(request.plan.keeps)}'")
        audio.append("asetpts=N/SR/TB")
    if cfg.audio.loudnorm:
        # Sin esto los clips suenan mas bajos que el resto del feed y el
        # espectador percibe el video como "peor" sin saber por que.
        audio.append(
            f"loudnorm=I={cfg.audio.target_i}:TP={cfg.audio.target_tp}"
            f":LRA={cfg.audio.target_lra}"
        )
    audio.append("aresample=48000")
    audio.append("aformat=sample_fmts=fltp:channel_layouts=stereo")
    chains.append(",".join(audio) + "[aout]")

    return ";\n".join(chains) + "\n"


def _reframe_filters(
    source: MediaInfo,
    target_width: int,
    target_height: int,
    track: Optional[Track],
    plan: JumpCutPlan,
    cfg,
) -> List[str]:
    """Filtros que convierten el plano 16:9 del podcast en el panel vertical.

    El orden es importante y no es negociable:

      1. `crop` recorta del original la mayor ventana que YA tiene la
         proporcion del panel, siguiendo la cara. Recortar, y no encajar, es lo
         que evita que la gente salga estirada.
      2. `scale` + `crop` llevan esa ventana al tamano exacto del panel sin
         deformarla (ver _cover).
      3. `zoompan` solo hace el punch-in, sobre una imagen que ya tiene la
         proporcion correcta.

    Meter el fotograma entero en zoompan y dejar que el encaje al panel -- que
    es lo que se hacia antes -- aplasta la imagen: zoompan recorta siempre con
    la proporcion de la ENTRADA y luego la escala a la de SALIDA, sin respetar
    ninguna de las dos.
    """
    crop_w, crop_h = _fit_crop(source, target_width, target_height)
    centre_y = source.height * 0.45      # las caras caen algo por encima del centro
    centre_x = _centre_expression(track, plan, source.display_width, time_var="t")

    x = f"clip({centre_x}-{crop_w / 2:.1f},0,in_w-{crop_w})"
    y = f"clip({centre_y:.1f}-{crop_h / 2:.1f},0,in_h-{crop_h})"
    # Hasta ffmpeg 7 hacia falta eval=frame para que x/y se recalculasen en
    # cada fotograma; en ffmpeg 8 es el comportamiento por defecto y la
    # opcion ya no existe.
    evaluation = ":eval=frame" if filter_supports("crop", "eval") else ""

    filters = [
        f"crop=w={crop_w}:h={crop_h}:x='{x}':y='{y}'{evaluation}",
        *_cover(target_width, target_height),
    ]
    if not cfg.zoom.enabled:
        return filters

    # zoompan escala la entrada por z y extrae una ventana de salida fija. Aqui
    # entrada y salida tienen el mismo tamano, asi que z es literalmente el
    # punch-in y x/y solo lo mantienen centrado. Ojo: dentro de zoompan NO
    # existe la variable `t`; el tiempo sale del contador de fotogramas `on`.
    # Y z se recorta a un minimo de 1: por debajo, zoompan lo ignora.
    clock = f"(on/{cfg.layout.fps})"
    low = max(1.0, cfg.zoom.min_zoom)
    high = max(low, cfg.zoom.max_zoom)
    punch = (f"({low}+{high - low}"
             f"*(0.5-0.5*cos(2*PI*{clock}/{cfg.zoom.period})))")
    filters.append(
        f"zoompan=z='{punch}':x='(iw-iw/zoom)/2':y='(ih-ih/zoom)/2'"
        f":d=1:s={target_width}x{target_height}:fps={cfg.layout.fps}"
    )
    return filters


def _square_pixels(source: MediaInfo) -> List[str]:
    """Convierte a pixeles cuadrados si el origen no los tiene.

    Sin esto, un video anamorfico se recorta con una geometria que no es la que
    se ve, y sale deformado hagamos lo que hagamos despues.
    """
    if source.square_pixels:
        return []
    return [f"scale={source.display_width}:{source.height}:flags=bicubic", "setsar=1"]


def _cover(target_width: int, target_height: int) -> List[str]:
    """Lleva la imagen al tamano exacto del panel SIN deformarla.

    `force_original_aspect_ratio=increase` escala hasta cubrir el panel
    manteniendo la proporcion, y el `crop` posterior se come el sobrante (unos
    pocos pixeles, porque la entrada ya viene recortada a la proporcion buena).
    Un `scale=W:H` a secas estiraria la imagen para que cuadrase, que es
    justo lo que no queremos.
    """
    return [
        f"scale={target_width}:{target_height}"
        f":force_original_aspect_ratio=increase:flags=bicubic",
        f"crop={target_width}:{target_height}",
    ]


def _center_crop(source: MediaInfo, target_width: int, target_height: int) -> str:
    crop_w, crop_h = _fit_crop(source, target_width, target_height)
    return f"crop=w={crop_w}:h={crop_h}:x=(in_w-{crop_w})/2:y=(in_h-{crop_h})/2"


def _fit_crop(source: MediaInfo, target_width: int, target_height: int) -> Tuple[int, int]:
    """Mayor recorte del origen que tiene la proporcion del panel destino.

    Se mide sobre la anchura de VISUALIZACION (pixeles cuadrados), que es la
    que tendra la imagen cuando llegue a este filtro.
    """
    source_width = source.display_width
    target_aspect = target_width / target_height
    if source.aspect > target_aspect:
        crop_h = source.height
        crop_w = int(round(crop_h * target_aspect))
    else:
        crop_w = source_width
        crop_h = int(round(crop_w / target_aspect))
    return _even(min(crop_w, source_width)), _even(min(crop_h, source.height))


def _centre_expression(track: Optional[Track], plan: JumpCutPlan,
                       source_width: int, time_var: str) -> str:
    """Expresion de ffmpeg con la x del centro del encuadre, en pixeles."""
    if track is None or not track.points:
        return f"{source_width / 2:.1f}"

    # La trayectoria se midio sobre el clip SIN cortar; el render la ve ya
    # recortada, asi que hay que trasladar cada instante.
    points: List[Tuple[float, float]] = []
    for t, cx in track.points:
        moment = plan.remap(t)
        if points and moment <= points[-1][0] + 1e-3:
            continue                      # cayo dentro de un hueco eliminado
        points.append((moment, cx * source_width))

    return _piecewise_linear(points, time_var)


def _piecewise_linear(points: Sequence[Tuple[float, float]], time_var: str) -> str:
    """Interpolacion lineal por tramos como expresion de ffmpeg."""
    if not points:
        return "0"
    if len(points) == 1:
        return f"{points[0][1]:.1f}"

    expression = f"{points[-1][1]:.1f}"          # despues del ultimo punto: fijo
    for (t0, x0), (t1, x1) in reversed(list(zip(points, points[1:]))):
        span = max(t1 - t0, 1e-6)
        slope = (x1 - x0) / span
        segment = f"({x0:.1f}+{slope:.4f}*({time_var}-{t0:.3f}))"
        expression = f"if(lt({time_var},{t1:.3f}),{segment},{expression})"

    # Antes del primer punto mantenemos el valor inicial.
    return (f"if(lt({time_var},{points[0][0]:.3f}),"
            f"{points[0][1]:.1f},{expression})")


def _select_expr(keeps: Sequence[Tuple[float, float]]) -> str:
    """`between(...)+between(...)`: en ffmpeg el + actua como OR logico."""
    return "+".join(f"between(t,{a:.3f},{b:.3f})" for a, b in keeps)


def _video_encoder_args(cfg_output) -> List[str]:
    """Argumentos del codificador, usando la GPU si de verdad la hay.

    La deteccion comprueba que el codificador funcione (ffmpeg lista
    h264_nvenc aunque no haya GPU NVIDIA) y guarda el resultado, asi que
    renderizar cinco clips no lanza cinco pruebas.
    """
    encoder = _resolve_encoder(cfg_output)
    crf = str(cfg_output.crf)

    if encoder == "h264_nvenc":
        return ["-c:v", encoder, "-preset", "p5", "-rc", "vbr",
                "-cq", crf, "-b:v", "0"]
    if encoder == "h264_qsv":
        return ["-c:v", encoder, "-preset", "medium", "-global_quality", crf]
    if encoder == "h264_amf":
        return ["-c:v", encoder, "-quality", "balanced", "-rc", "cqp",
                "-qp_i", crf, "-qp_p", crf, "-qp_b", crf]
    if encoder and encoder != cfg_output.video_codec:
        # Un codificador que ha pedido el usuario y no conocemos: se le pasa
        # tal cual con una calidad razonable.
        return ["-c:v", encoder, "-b:v", "6M"]

    return ["-c:v", cfg_output.video_codec,
            "-preset", cfg_output.preset,
            "-crf", crf]


_ENCODER_CHOICE: Optional[str] = None


def _resolve_encoder(cfg_output) -> str:
    """Nombre del codificador a usar. Se decide una vez por ejecucion."""
    global _ENCODER_CHOICE
    if _ENCODER_CHOICE is not None:
        return _ENCODER_CHOICE

    preference = getattr(cfg_output, "hardware_encoder", "auto")
    if getattr(cfg_output, "use_nvenc", False):
        preference = "h264_nvenc"          # compatibilidad con configs viejas

    from .ffmpeg_utils import detect_hardware_encoder
    _ENCODER_CHOICE = detect_hardware_encoder(preference) or cfg_output.video_codec
    if _ENCODER_CHOICE == cfg_output.video_codec:
        log.info("Codificando por CPU con %s (preset %s).",
                 cfg_output.video_codec, cfg_output.preset)
    return _ENCODER_CHOICE


def _even(value: int) -> int:
    """Los codecs 4:2:0 exigen dimensiones pares."""
    return value - (value % 2)


def extract_thumbnail(video: Path, at: float = 1.2) -> Optional[Path]:
    """Guarda un fotograma como portada, junto al short.

    Es la imagen que pide YouTube al subir un short, y la que se ve en la
    galeria de resultados sin tener que cargar el video entero. Si falla no
    pasa nada: el short ya esta hecho.
    """
    output = video.with_suffix(".jpg")
    try:
        run(["-ss", f"{max(0.0, at):.3f}", "-i", str(video.absolute()),
             "-frames:v", "1", "-q:v", "3", str(output.absolute())],
            desc=f"portada {video.name}")
    except FFmpegError as exc:
        log.debug("No se pudo sacar la portada de %s: %s", video.name, exc)
        return None
    return output if output.is_file() else None
