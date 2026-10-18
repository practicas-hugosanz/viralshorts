"""Configuracion del pipeline: valores por defecto + carga desde YAML.

Todos los parametros ajustables viven aqui. `Config.load()` mezcla en
profundidad el YAML del usuario sobre los valores por defecto, de modo que el
fichero de configuracion solo necesita contener lo que se quiera cambiar.
"""

from __future__ import annotations

import copy
from dataclasses import dataclass, field, fields, is_dataclass
from pathlib import Path
from typing import Any, Dict, List


# ----------------------------------------------------------------------------
# Grupos de configuracion
# ----------------------------------------------------------------------------

@dataclass
class DownloadConfig:
    # Preferimos <=1080p: bajar 4K de un podcast de 3h no aporta nada al short.
    # Con 720 la descarga tarda la mitad y el panel de arriba pierde un pelo de
    # nitidez (el short recorta un trozo del 16:9 y lo escala a 1080 de ancho).
    max_height: int = 1080
    format: str = ("bestvideo[height<={max_height}][ext=mp4]+bestaudio[ext=m4a]/"
                   "best[height<={max_height}]/best")
    # Navegador del que leer cookies para videos con restriccion de edad o login.
    # Ej: "chrome", "firefox", "edge". None = sin cookies.
    cookies_from_browser: str | None = None
    cookiefile: str | None = None
    retries: int = 5
    # yt-dlp baja los fragmentos DASH de uno en uno por defecto. Con 8 en
    # paralelo la descarga de un podcast de 3h pasa de minutos a segundos en
    # una conexion normal; es la mejora mas barata de todo el pipeline.
    concurrent_fragments: int = 8
    # Trozos de 10 MB: evita que una conexion que se corta obligue a repetir
    # la descarga entera del fragmento.
    http_chunk_size: int = 10_485_760


@dataclass
class TranscribeConfig:
    model_size: str = "medium"       # tiny | base | small | medium | large-v3
    language: str | None = "es"      # None = autodetectar
    device: str = "auto"             # auto | cpu | cuda
    compute_type: str = "auto"       # auto | int8 | float16 | float32
    # beam_size 1 (greedy) transcribe ~2x mas rapido que 5 y para elegir
    # momentos y poner subtitulos la diferencia de calidad es despreciable.
    beam_size: int = 1
    vad_filter: bool = True
    # Hilos de CPU para ctranslate2. 0 = auto (nucleos fisicos de la maquina).
    # El valor por defecto de la libreria es 4, que deja media CPU parada.
    cpu_threads: int = 0
    # Arrastrar el texto anterior como contexto es lento y en podcasts largos
    # provoca bucles de repeticion. Desactivarlo acelera y estabiliza.
    condition_on_previous_text: bool = False
    # faster-whisper >= 1.1 puede transcribir varios trozos a la vez
    # (BatchedInferencePipeline): 2-4x mas rapido con el mismo modelo y la
    # misma calidad. Si la version instalada no lo trae, se usa la normal.
    batched: bool = True
    batch_size: int = 8


@dataclass
class PrescanConfig:
    """Preseleccion de zonas SIN transcribir.

    El heatmap de YouTube y la energia del audio se calculan en segundos sobre
    el video entero. Con esas dos senales ya sabemos donde estan los momentos
    calientes, asi que solo hace falta transcribir esas zonas y no las tres
    horas enteras: es la diferencia entre 100 minutos y 10.
    """
    enabled: bool = True
    # Zonas candidatas a transcribir. 0 = auto (num_clips * 5, minimo 12).
    # Mas zonas = mejor seleccion final pero mas tiempo de transcripcion.
    regions: int = 0
    # Margen extra a cada lado de la zona, para que el clip pueda respirar y
    # cortar en frontera de frase de verdad.
    padding: float = 15.0
    # Paso del barrido grueso, en segundos.
    step: float = 5.0


@dataclass
class ClipsConfig:
    num_clips: int = 3
    min_duration: float = 25.0
    max_duration: float = 55.0
    # Descarta los primeros/ultimos segundos del video (intros, patrocinios,
    # despedidas): casi nunca contienen el mejor momento.
    head_skip: float = 60.0
    tail_skip: float = 30.0
    # Separacion minima entre dos clips seleccionados.
    min_gap: float = 30.0


@dataclass
class ScoringConfig:
    # Pesos de cada senal. Se normalizan solos, no hace falta que sumen 1.
    weight_heatmap: float = 0.30     # "most replayed" de YouTube
    weight_energy: float = 0.15      # energia del audio (risas, subidas de voz)
    weight_density: float = 0.05     # palabras por segundo
    # Cuanto se entiende el clip sin haber visto el resto del podcast.
    # Pesa mucho a proposito: un clip incomprensible no lo salva el heatmap.
    weight_context: float = 0.25
    weight_llm: float = 0.35         # puntuacion de contenido del LLM
    weight_hook: float = 0.15        # fuerza del gancho inicial (LLM)
    # Cuantas ventanas candidatas se mandan al LLM (las mejores por senales
    # baratas). Mas candidatas = mejor seleccion pero mas coste.
    llm_candidates: int = 24
    # Dos clips del mismo argumento aburren aunque esten separados en el
    # tiempo. Si comparten mas de esta fraccion de vocabulario, el segundo se
    # descarta. 1.0 desactiva la comprobacion.
    max_similarity: float = 0.5


@dataclass
class LLMConfig:
    enabled: bool = True
    model: str = "claude-opus-5-5"
    effort: str = "medium"           # low | medium | high | xhigh | max
    max_tokens: int = 16000
    # Generar titulo, texto de gancho, descripcion y hashtags por clip.
    generate_metadata: bool = True
    # Un 429 o un "overloaded" puntual no puede tirar la puntuacion entera.
    max_retries: int = 3
    retry_delay: float = 3.0         # segundos; se duplica en cada intento
    # Fragmentos por peticion. Mandarlos todos de golpe hace que la respuesta
    # se corte por max_tokens y se pierdan clips sin puntuar.
    batch_size: int = 10
    metadata_batch_size: int = 6
    # Guarda en disco lo que ya ha puntuado: relanzar el mismo video no vuelve
    # a pagar por los mismos fragmentos.
    cache: bool = True


@dataclass
class ContextConfig:
    """Deteccion de donde EMPIEZA un tema, para que el clip se entienda solo."""
    enabled: bool = True
    block_seconds: float = 12.0      # tamano del bloque para comparar vocabulario
    compare_blocks: int = 5          # bloques a cada lado de la posible frontera
    sensitivity: float = 0.6         # desviaciones tipicas para marcar frontera
    boundary_grace: float = 8.0      # margen tras la frontera que sigue valiendo
    intro_seconds: float = 15.0      # plazo para que el clip nombre su asunto


@dataclass
class MetadataConfig:
    """Textos de publicacion cuando NO hay modelo de lenguaje.

    Se extraen del propio clip (ver metadata.py). Tambien rellenan los huecos
    que deje Claude si responde a medias.
    """
    enabled: bool = True
    max_title: int = 70
    max_hook: int = 45              # cabe en el rotulo sin comerse el plano
    max_description: int = 160
    hashtags: int = 6
    # Segundos iniciales de los que puede salir el gancho: prometer algo que se
    # dice al final del clip es mentirle al espectador.
    hook_window: float = 12.0
    # Nota minima para que una frase merezca salir como rotulo de gancho. Por
    # debajo no se pone ninguno: un rotulo malo tapa el video sin aportar nada.
    min_hook_score: float = 3.5
    # Igual para el titulo: por debajo se usa el nombre del programa, que
    # nunca queda raro, en vez de una frase suelta que no pega.
    min_title_score: float = 2.4


@dataclass
class ReframeConfig:
    """Reencuadre del panel superior siguiendo al que habla."""
    enabled: bool = True
    sample_interval: float = 0.5     # segundos entre muestras de deteccion
    smoothing_window: float = 2.0    # ventana de suavizado en segundos
    deadzone: float = 0.04           # % de anchura que hay que moverse para reencuadrar
    max_keyframes: int = 60          # tope de tramos en la expresion de ffmpeg
    # Un podcast mete fotos, carteles y miniaturas con caras dentro del plano.
    # El detector las ve igual que a una persona, y el encuadre se va a la foto
    # en vez de a quien esta hablando. Se distinguen por lo unico que las
    # separa: la persona se mueve, la foto no.
    ignore_static_faces: bool = True
    min_face_motion: float = 1.6     # niveles de gris de cambio entre muestras
    static_ratio: float = 0.35       # fraccion del movimiento de la cara mas viva
    min_face_size: float = 0.06      # tamano minimo de cara (solo detector Haar)
    # Con varias caras en plano, seguir a la que MUEVE LA BOCA mientras suena
    # audio en vez de a la mas grande o la mas cercana. Es la diferencia entre
    # encuadrar al que habla y encuadrar al que escucha.
    speaker_tracking: bool = True
    talk_window: float = 2.5         # segundos de historial de "quien habla"
    switch_margin: float = 1.4       # cuanto tiene que ganar el otro para saltar
    switch_hold: int = 2             # muestras seguidas ganando antes de saltar
    score_threshold: float = 0.6     # confianza minima de YuNet
    # Modelo YuNet. Se descarga solo la primera vez a paths.cache_dir; pon una
    # ruta en model_path si prefieres tenerlo fijo o trabajar sin conexion.
    model_path: str | None = None
    model_url: str = (
        "https://github.com/opencv/opencv_zoo/raw/main/models/"
        "face_detection_yunet/face_detection_yunet_2023mar.onnx"
    )


@dataclass
class ZoomConfig:
    """Punch-in suave para que el plano no se quede estatico."""
    enabled: bool = True
    min_zoom: float = 1.0
    max_zoom: float = 1.07
    period: float = 14.0             # segundos por ciclo completo de zoom


@dataclass
class JumpCutsConfig:
    """Eliminacion de silencios para subir el ritmo."""
    enabled: bool = True
    max_silence: float = 0.35        # silencio maximo que se conserva
    padding: float = 0.12            # margen alrededor de cada palabra
    min_keep: float = 0.20           # descarta trozos conservados mas cortos que esto


@dataclass
class LayoutConfig:
    width: int = 1080
    height: int = 1920
    top_ratio: float = 0.5           # proporcion de altura para el panel del podcast
    fps: int = 30


@dataclass
class AudioConfig:
    loudnorm: bool = True
    target_i: float = -14.0          # LUFS objetivo (estandar de redes sociales)
    target_tp: float = -1.5          # true peak
    target_lra: float = 11.0


@dataclass
class SubtitlesConfig:
    enabled: bool = True
    font_name: str = "Arial Black"
    font_size: int = 92
    uppercase: bool = True
    color_normal: str = "#FFFFFF"
    color_active: str = "#FFF200"
    color_outline: str = "#000000"
    color_shadow: str = "#000000"
    outline_width: int = 5
    shadow_depth: int = 2
    max_words_per_group: int = 3
    vertical_position_ratio: float = 0.30   # 0 = abajo del todo, 1 = arriba
    remove_punctuation: bool = True
    punctuation_chars: str = ".,!?¡¿…\";:"
    pop_scale: int = 118             # % de escala de la palabra activa (100 = sin pop)
    pop_duration: float = 0.12       # duracion del pop en segundos
    # Tapa una letra de las palabrotas en el subtitulo: "puta" -> "p#ta". El
    # audio no se toca; esto es solo para que el texto no sea literal.
    censor_profanity: bool = True
    censor_char: str = "#"
    # Palabras propias que censurar ademas de la lista de serie. Se tratan como
    # raices: "cabron" tapa tambien "cabronazo".
    censor_words: List[str] = field(default_factory=list)
    fonts_dir: str | None = None     # carpeta con .ttf propios (opcional)


@dataclass
class HookConfig:
    """Titular fijo en la parte superior durante los primeros segundos."""
    enabled: bool = True
    duration: float = 4.0
    font_size: int = 64
    font_name: str = "Arial Black"
    color: str = "#FFFFFF"
    color_outline: str = "#000000"
    top_margin_ratio: float = 0.06
    uppercase: bool = True
    max_chars: int = 70


@dataclass
class TitleCardConfig:
    """Tarjeta de contexto al principio del video.

    Va en el CENTRO del encuadre vertical, justo en la costura entre el panel
    del podcast y el del gameplay, con el mismo estilo que los subtitulos.
    Ocupa el sitio mas visible que hay, y solo durante los primeros segundos.

    Nunca coincide en pantalla con el gancho: el gancho empieza cuando esta se
    va (ver subtitles.build_ass).
    """
    enabled: bool = True
    # De donde sale el texto:
    #   video  - el titulo del video de origen, entero
    #   show   - programa e invitado ("The Wild Project - Xokas"): el titulo
    #            del video sin la lista de temas del episodio
    #   clip   - la mejor frase del propio clip (ver metadata.py)
    source: str = "show"
    duration: float = 2.5            # lo que tarda en leerse una linea corta
    fade: float = 0.25               # entrada y salida por desvanecido
    font_name: str = ""              # vacio = la misma que los subtitulos
    font_size: int = 74
    uppercase: bool = False          # en minusculas se lee mas rapido
    color: str = "#FFFFFF"
    color_outline: str = "#000000"
    outline_width: int = 6
    shadow_depth: int = 2
    max_chars: int = 90
    wrap_chars: int = 22


@dataclass
class OutputConfig:
    dir: str = "output"
    crf: int = 20
    preset: str = "medium"
    video_codec: str = "libx264"
    audio_bitrate: str = "192k"
    # Codificador por hardware. "auto" prueba NVENC, QSV y AMF (y comprueba
    # que funcionen de verdad, no solo que ffmpeg los liste) y cae a libx264
    # si no hay ninguno. "off" fuerza libx264. Tambien admite un nombre
    # concreto, p. ej. "h264_nvenc".
    hardware_encoder: str = "auto"
    # Compatibilidad: si esta en True equivale a hardware_encoder: h264_nvenc.
    use_nvenc: bool = False
    # Renders simultaneos. 0 = auto segun los nucleos. ffmpeg ya usa varios
    # hilos, pero deja la CPU a medias en las partes seriales del filtrado.
    render_workers: int = 0
    # Fotograma de portada (.jpg) junto a cada short, para la miniatura.
    thumbnail: bool = True
    thumbnail_at: float = 1.2        # segundo del que se saca la portada
    # Hoja de calculo con los textos de publicacion de todos los shorts.
    csv: bool = True
    # Una subcarpeta por ejecucion dentro de `dir`. En False todo va a `dir` y
    # los shorts nuevos pisan a los de la ejecucion anterior.
    run_subfolder: bool = True


@dataclass
class PathsConfig:
    work_dir: str = "work"           # descargas + cache de transcripciones
    cache_dir: str = "work/cache"
    plans_dir: str = "work/plans"    # selecciones guardadas para revisar


@dataclass
class Config:
    download: DownloadConfig = field(default_factory=DownloadConfig)
    transcribe: TranscribeConfig = field(default_factory=TranscribeConfig)
    prescan: PrescanConfig = field(default_factory=PrescanConfig)
    clips: ClipsConfig = field(default_factory=ClipsConfig)
    scoring: ScoringConfig = field(default_factory=ScoringConfig)
    context: ContextConfig = field(default_factory=ContextConfig)
    llm: LLMConfig = field(default_factory=LLMConfig)
    metadata: MetadataConfig = field(default_factory=MetadataConfig)
    reframe: ReframeConfig = field(default_factory=ReframeConfig)
    zoom: ZoomConfig = field(default_factory=ZoomConfig)
    jumpcuts: JumpCutsConfig = field(default_factory=JumpCutsConfig)
    layout: LayoutConfig = field(default_factory=LayoutConfig)
    audio: AudioConfig = field(default_factory=AudioConfig)
    subtitles: SubtitlesConfig = field(default_factory=SubtitlesConfig)
    hook: HookConfig = field(default_factory=HookConfig)
    title_card: TitleCardConfig = field(default_factory=TitleCardConfig)
    output: OutputConfig = field(default_factory=OutputConfig)
    paths: PathsConfig = field(default_factory=PathsConfig)

    # ------------------------------------------------------------------
    @classmethod
    def load(cls, path: str | Path | None) -> "Config":
        cfg = cls()
        if path is None:
            return cfg

        p = Path(path)
        if not p.is_file():
            raise FileNotFoundError(f"No se encontro el fichero de configuracion: {p}")

        try:
            import yaml
        except ImportError as exc:  # pragma: no cover
            raise ImportError("Falta PyYAML. Ejecuta: pip install pyyaml") from exc

        data = yaml.safe_load(p.read_text(encoding="utf-8")) or {}
        if not isinstance(data, dict):
            raise ValueError(f"{p} debe contener un mapa YAML en la raiz.")

        _apply(cfg, data, path=p.name)
        return cfg

    def to_dict(self) -> Dict[str, Any]:
        return _to_dict(self)


# ----------------------------------------------------------------------------
# Utilidades internas
# ----------------------------------------------------------------------------

def _apply(target: Any, data: Dict[str, Any], path: str) -> None:
    """Vuelca `data` sobre un dataclass, validando que las claves existan."""
    known = {f.name: f for f in fields(target)}
    for key, value in data.items():
        if key not in known:
            raise ValueError(
                f"{path}: clave desconocida '{key}' en {type(target).__name__}. "
                f"Validas: {', '.join(sorted(known))}"
            )
        current = getattr(target, key)
        if is_dataclass(current) and isinstance(value, dict):
            _apply(current, value, path)
        else:
            setattr(target, key, copy.deepcopy(value))


def _to_dict(obj: Any) -> Any:
    if is_dataclass(obj):
        return {f.name: _to_dict(getattr(obj, f.name)) for f in fields(obj)}
    if isinstance(obj, list):
        return [_to_dict(v) for v in obj]
    return obj
