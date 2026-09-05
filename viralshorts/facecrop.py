"""Reencuadre inteligente: seguir la cara en vez de recortar por el centro.

Un podcast se graba en 16:9 con dos o tres personas repartidas por el plano.
Recortar por el centro (lo que hacia el script original) parte a los invitados
por la mitad. Aqui muestreamos el clip, detectamos caras y construimos una
trayectoria suave del centro del encuadre, que despues se traduce a una
expresion de ffmpeg.

Se usa YuNet (detector DNN que viene con OpenCV) porque aguanta caras de
perfil, con gafas y mal iluminadas, que es exactamente lo que hay en un
podcast. Si no se puede, se cae al clasificador Haar de OpenCV 4, y si tampoco,
al recorte centrado de siempre: el pipeline nunca se rompe por esto.
"""

from __future__ import annotations

import hashlib
import json
import logging
import threading
import urllib.request
from dataclasses import dataclass
from pathlib import Path
from typing import List, Optional, Sequence, Tuple

log = logging.getLogger(__name__)

# Ancho al que reducimos cada fotograma antes de detectar: la deteccion no
# necesita 1080p y a 480 va varias veces mas rapido.
_DETECT_WIDTH = 480

# Una sola descarga del modelo aunque haya varios renders en marcha.
_MODEL_LOCK = threading.Lock()

_YUNET_URL = (
    "https://github.com/opencv/opencv_zoo/raw/main/models/"
    "face_detection_yunet/face_detection_yunet_2023mar.onnx"
)


@dataclass
class Track:
    """Trayectoria del centro horizontal del encuadre, normalizada 0..1."""
    points: List[Tuple[float, float]]   # (t relativo al clip, cx normalizado)
    detection_ratio: float              # fraccion de muestras con cara detectada

    @property
    def is_useful(self) -> bool:
        return len(self.points) >= 2 and self.detection_ratio >= 0.15


def track_faces(video: Path, start: float, duration: float, cfg,
                cache_dir: Optional[Path] = None,
                energy: Optional["object"] = None) -> Optional[Track]:
    """Devuelve la trayectoria del encuadre para [start, start+duration).

    `energy` es la curva de energia del audio del podcast entero (opcional).
    Con ella el seguimiento distingue al que habla del que escucha: cuando no
    suena nada, el encuadre se queda donde esta en vez de irse detras del
    primero que se rasque la nariz.
    """
    if not cfg.enabled:
        return None

    cache_path = _cache_path(video, start, duration, cfg, cache_dir)
    cached = _read_cache(cache_path)
    if cached is not None:
        log.info("Reencuadre en cache (%d puntos).", len(cached.points))
        return cached if cached.points else None

    try:
        import cv2
    except ImportError:
        log.warning("OpenCV no esta instalado; se usara recorte centrado. "
                    "Instalalo con: pip install opencv-python-headless")
        return None

    detector = _make_detector(cv2, cfg, cache_dir)
    if detector is None:
        return None

    capture = cv2.VideoCapture(str(video))
    if not capture.isOpened():
        log.warning("OpenCV no pudo abrir %s; se usara recorte centrado.", video.name)
        return None

    try:
        samples = _sample(cv2, capture, detector, start, duration, cfg)
    finally:
        capture.release()

    if not samples:
        log.info("No se detecto ninguna cara; se usara recorte centrado.")
        _write_cache(cache_path, None)
        return None

    samples = _drop_static_faces(samples, cfg)
    centres = _choose_centres(samples, cfg, energy, start)

    ratio = sum(1 for _, cx in centres if cx is not None) / len(centres)
    filled = _fill_gaps(centres)
    if filled is None:
        log.info("No se detecto ninguna cara; se usara recorte centrado.")
        _write_cache(cache_path, None)
        return None

    smoothed = _smooth(filled, cfg.smoothing_window, cfg.sample_interval)
    track = Track(
        points=_to_keyframes(smoothed, cfg.deadzone, cfg.max_keyframes),
        detection_ratio=ratio,
    )

    if not track.is_useful:
        log.info("Deteccion poco fiable (%.0f%% de las muestras); "
                 "se usara recorte centrado.", ratio * 100)
        _write_cache(cache_path, None)
        return None

    log.info("Reencuadre: cara detectada en el %.0f%% de las muestras, "
             "%d puntos de trayectoria.", ratio * 100, len(track.points))
    _write_cache(cache_path, track)
    return track


# ----------------------------------------------------------------------------
# Cache de trayectorias
# ----------------------------------------------------------------------------
#
# Detectar caras es lo mas lento del montaje de un clip despues de codificar.
# Y se repite entero cada vez que se vuelve a montar el mismo momento: al
# cambiar un titulo, al retocar el estilo de los subtitulos, al rehacer un
# short desde la interfaz. La trayectoria depende solo del video, del tramo y
# de los ajustes de reencuadre, asi que se guarda.

def _cache_path(video: Path, start: float, duration: float, cfg,
                cache_dir: Optional[Path]) -> Optional[Path]:
    if cache_dir is None:
        return None
    try:
        stat = Path(video).stat()
    except OSError:
        return None

    firma = "|".join(str(x) for x in (
        Path(video).resolve(), stat.st_size, int(stat.st_mtime),
        round(start, 2), round(duration, 2),
        cfg.sample_interval, cfg.smoothing_window, cfg.deadzone,
        cfg.max_keyframes, cfg.ignore_static_faces, cfg.min_face_motion,
        cfg.static_ratio, cfg.score_threshold,
        getattr(cfg, "speaker_tracking", False),
        getattr(cfg, "talk_window", 0), getattr(cfg, "switch_margin", 0),
        getattr(cfg, "switch_hold", 0),
    ))
    nombre = hashlib.sha1(firma.encode("utf-8")).hexdigest()[:16]
    return Path(cache_dir) / f"reframe_{nombre}.json"


def _read_cache(path: Optional[Path]) -> Optional[Track]:
    if path is None or not path.is_file():
        return None
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
        return Track(points=[(float(t), float(cx)) for t, cx in data["puntos"]],
                     detection_ratio=float(data["deteccion"]))
    except (OSError, ValueError, KeyError, TypeError):
        return None


def _write_cache(path: Optional[Path], track: Optional[Track]) -> None:
    """Guarda la trayectoria. Un clip sin caras se guarda como lista vacia:
    tambien cuesta minutos averiguarlo y tampoco hay que repetirlo."""
    if path is None:
        return
    if track is None:
        data = {"puntos": [], "deteccion": 0.0}
    else:
        data = {"puntos": [[round(t, 3), round(cx, 5)] for t, cx in track.points],
                "deteccion": round(track.detection_ratio, 4)}
    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(data), encoding="utf-8")
    except OSError as exc:
        log.debug("No se pudo guardar el reencuadre en cache: %s", exc)


# ----------------------------------------------------------------------------
# Detectores
# ----------------------------------------------------------------------------

@dataclass
class Face:
    """Una cara detectada en un fotograma reducido."""
    cx: float                      # centro horizontal, normalizado 0..1
    area: float                    # en pixeles del fotograma reducido
    box: Tuple[int, int, int, int] # (x, y, w, h) en el fotograma reducido
    motion: float = 0.0            # cuanto cambia esa zona entre muestras
    mouth: float = 0.0             # cuanto cambia el TERCIO INFERIOR de la cara

    @property
    def bucket(self) -> int:
        """Identidad aproximada: quien esta sentado en ese sitio del plano."""
        return round(self.cx / 0.06)


class _Detector:
    """Devuelve las caras de un fotograma."""

    def detect(self, frame) -> List[Face]:
        raise NotImplementedError


class _YuNetDetector(_Detector):
    def __init__(self, cv2, model_path: Path, size: Tuple[int, int], threshold: float):
        self._cv2 = cv2
        self._net = cv2.FaceDetectorYN.create(
            str(model_path), "", size, threshold, 0.3, 5000
        )
        self._net.setInputSize(size)
        self._width, self._height = size

    def detect(self, frame) -> List[Face]:
        ok, faces = self._net.detect(frame)
        if not ok or faces is None:
            return []
        return [
            Face(cx=(float(x) + float(w) / 2) / self._width,
                 area=float(w) * float(h),
                 box=(int(x), int(y), int(w), int(h)))
            for x, y, w, h in faces[:, :4]
        ]


class _HaarDetector(_Detector):
    """Respaldo para OpenCV 4: peor con perfiles, pero no necesita descarga."""

    def __init__(self, cv2, cascade, width: int, min_side: int):
        self._cv2 = cv2
        self._cascade = cascade
        self._width = width
        self._min_side = min_side

    def detect(self, frame) -> List[Face]:
        gray = self._cv2.cvtColor(frame, self._cv2.COLOR_BGR2GRAY)
        gray = self._cv2.equalizeHist(gray)
        faces = self._cascade.detectMultiScale(
            gray, scaleFactor=1.15, minNeighbors=6,
            minSize=(self._min_side, self._min_side),
        )
        return [
            Face(cx=(x + w / 2) / self._width, area=float(w * h),
                 box=(int(x), int(y), int(w), int(h)))
            for x, y, w, h in faces
        ]


def _make_detector(cv2, cfg, cache_dir: Optional[Path]) -> Optional[_Detector]:
    size = (_DETECT_WIDTH, _DETECT_WIDTH)   # el tamano real se fija al muestrear

    if hasattr(cv2, "FaceDetectorYN"):
        model = _ensure_yunet_model(cfg, cache_dir)
        if model is not None:
            try:
                return _YuNetDetector(cv2, model, size, cfg.score_threshold)
            except Exception as exc:
                log.warning("No se pudo iniciar YuNet (%s); se prueba con Haar.", exc)

    cascade = _load_haar(cv2, cfg)
    if cascade is not None:
        return cascade

    log.warning("Ningun detector de caras disponible; se usara recorte centrado.")
    return None


def _ensure_yunet_model(cfg, cache_dir: Optional[Path]) -> Optional[Path]:
    """Ruta local al modelo YuNet, descargandolo la primera vez.

    Con varios clips montandose a la vez esto se llama desde varios hilos: sin
    el cerrojo, dos descargas escriben en el mismo fichero y el modelo que
    queda no lo puede abrir nadie.
    """
    if cfg.model_path:
        path = Path(cfg.model_path)
        if path.is_file():
            return path
        log.warning("reframe.model_path apunta a %s, que no existe.", path)

    target_dir = Path(cache_dir) if cache_dir else Path("work/cache")
    target = target_dir / "face_detection_yunet.onnx"
    if target.is_file() and target.stat().st_size > 10_000:
        return target

    with _MODEL_LOCK:
        if target.is_file() and target.stat().st_size > 10_000:
            return target                 # lo ha bajado el otro hilo mientras

        target_dir.mkdir(parents=True, exist_ok=True)
        log.info("Descargando el modelo de deteccion de caras "
                 "(una sola vez, ~230 KB)...")
        # A un fichero temporal y luego renombrar: si la descarga se corta a
        # medias no queda un .onnx a medio bajar que parezca valido.
        partial = target.with_suffix(".onnx.part")
        try:
            urllib.request.urlretrieve(cfg.model_url, partial)
            partial.replace(target)
        except Exception as exc:
            log.warning("No se pudo descargar el modelo de caras (%s).", exc)
            partial.unlink(missing_ok=True)
            return None
        return target


def _load_haar(cv2, cfg) -> Optional[_Detector]:
    """Clasificador Haar, solo presente hasta OpenCV 4."""
    if not hasattr(cv2, "CascadeClassifier") or not hasattr(cv2, "data"):
        return None
    path = Path(cv2.data.haarcascades) / "haarcascade_frontalface_default.xml"
    if not path.is_file():
        return None
    cascade = cv2.CascadeClassifier(str(path))
    if cascade.empty():
        return None
    log.info("Usando el detector Haar de OpenCV (menos preciso que YuNet).")
    return _HaarDetector(cv2, cascade, _DETECT_WIDTH,
                         max(20, int(_DETECT_WIDTH * cfg.min_face_size)))


# ----------------------------------------------------------------------------
# Muestreo y suavizado
# ----------------------------------------------------------------------------

def _sample(cv2, capture, detector: _Detector, start: float, duration: float, cfg
            ) -> List[Tuple[float, List[Face]]]:
    """Muestrea el clip y devuelve TODAS las caras de cada muestra.

    A cada cara se le mide ademas cuanto cambia su zona respecto a la muestra
    anterior. Esa es la unica diferencia observable entre una persona hablando
    y la foto de una persona pegada en la pared o metida en el montaje: la
    persona se mueve, la foto no.
    """
    fps = capture.get(cv2.CAP_PROP_FPS) or 30.0
    step = max(1, int(round(cfg.sample_interval * fps)))
    first_frame = int(round(start * fps))
    last_frame = int(round((start + duration) * fps))

    capture.set(cv2.CAP_PROP_POS_FRAMES, first_frame)

    samples: List[Tuple[float, List[Face]]] = []
    frame_index = first_frame
    sized = False
    previous_gray = None

    while frame_index < last_frame:
        if (frame_index - first_frame) % step:
            # Los fotogramas que no se miran se saltan con grab(), que los lee
            # del contenedor pero NO los decodifica. Con una muestra cada medio
            # segundo eso son 14 de cada 15 fotogramas que no se decodifican:
            # el muestreo pasa a ir varias veces mas rapido.
            if not capture.grab():
                break
            frame_index += 1
            continue

        ok, frame = capture.read()
        if not ok:
            break

        small = _shrink(cv2, frame)
        if not sized:
            _resize_detector(detector, small)
            sized = True
        gray = cv2.cvtColor(small, cv2.COLOR_BGR2GRAY)
        faces = detector.detect(small)
        if previous_gray is not None and previous_gray.shape == gray.shape:
            for face in faces:
                face.motion = _region_motion(cv2, previous_gray, gray, face.box)
                face.mouth = _region_motion(cv2, previous_gray, gray,
                                            _mouth_box(face.box))
        samples.append(((frame_index - first_frame) / fps, faces))
        previous_gray = gray
        frame_index += 1

    return samples


def _mouth_box(box: Tuple[int, int, int, int]) -> Tuple[int, int, int, int]:
    """Tercio inferior de la cara: donde se nota si esa persona esta hablando.

    La cara entera se mueve tambien cuando alguien asiente o se rie de lo que
    dice otro. La boca solo se mueve cuando habla.
    """
    x, y, w, h = box
    return (x + w // 6, y + int(h * 0.60), max(2, w - w // 3), max(2, int(h * 0.40)))


def _region_motion(cv2, before, after, box: Tuple[int, int, int, int]) -> float:
    """Cambio medio de luminosidad dentro de la caja, en niveles de gris."""
    import numpy as np

    x, y, w, h = box
    height, width = after.shape[:2]
    x0, y0 = max(0, x), max(0, y)
    x1, y1 = min(width, x + w), min(height, y + h)
    if x1 - x0 < 4 or y1 - y0 < 4:
        return 0.0

    patch_before = before[y0:y1, x0:x1].astype("int16")
    patch_after = after[y0:y1, x0:x1].astype("int16")
    return float(np.mean(np.abs(patch_after - patch_before)))


def _drop_static_faces(samples: List[Tuple[float, List[Face]]], cfg
                       ) -> List[Tuple[float, List[Face]]]:
    """Quita las caras que en realidad son fotos, carteles o miniaturas.

    Se agrupan las detecciones por posicion horizontal y se mira el movimiento
    TIPICO de cada grupo a lo largo del clip, no el de un fotograma suelto: un
    invitado callado tambien se queda quieto un segundo, pero no un minuto.

    Si todas las caras del clip estan quietas (el video corta a una foto a
    pantalla completa, por ejemplo) no se descarta ninguna: es mejor encuadrar
    la foto que perder el reencuadre entero.
    """
    if not cfg.ignore_static_faces:
        return samples

    buckets: dict = {}
    for _, faces in samples:
        for face in faces:
            buckets.setdefault(round(face.cx / 0.06), []).append(face.motion)

    # Un grupo con dos apariciones no dice nada; hace falta presencia sostenida.
    steady = {k: _median(v) for k, v in buckets.items() if len(v) >= 4}
    if not steady:
        return samples

    liveliest = max(steady.values())
    if liveliest < cfg.min_face_motion:
        return samples          # aqui no se mueve nada: no hay con que comparar

    cut = max(cfg.min_face_motion, liveliest * cfg.static_ratio)
    dead = {k for k, value in steady.items() if value < cut}
    if not dead:
        return samples

    log.info("Reencuadre: %d cara(s) descartadas por estar inmoviles "
             "(fotos o carteles, no gente hablando).", len(dead))
    return [
        (t, [f for f in faces if round(f.cx / 0.06) not in dead])
        for t, faces in samples
    ]


def _median(values: Sequence[float]) -> float:
    ordered = sorted(values)
    middle = len(ordered) // 2
    if len(ordered) % 2:
        return ordered[middle]
    return (ordered[middle - 1] + ordered[middle]) / 2


def _shrink(cv2, frame):
    height, width = frame.shape[:2]
    if width <= _DETECT_WIDTH:
        return frame
    scale = _DETECT_WIDTH / width
    return cv2.resize(frame, (_DETECT_WIDTH, max(2, int(height * scale))))


def _resize_detector(detector: _Detector, frame) -> None:
    if isinstance(detector, _YuNetDetector):
        height, width = frame.shape[:2]
        detector._width, detector._height = width, height
        detector._net.setInputSize((width, height))


def _choose_centres(samples: Sequence[Tuple[float, List[Face]]],
                    cfg=None, energy: Optional["object"] = None,
                    clip_start: float = 0.0
                    ) -> List[Tuple[float, Optional[float]]]:
    """De todas las caras de cada muestra, a cual seguimos."""
    if cfg is not None and getattr(cfg, "speaker_tracking", False):
        speaking = _speaker_centres(samples, cfg, energy, clip_start)
        if speaking is not None:
            return speaking

    centres: List[Tuple[float, Optional[float]]] = []
    previous_cx: Optional[float] = None
    for moment, faces in samples:
        cx = _pick(faces, previous_cx)
        centres.append((moment, cx))
        if cx is not None:
            previous_cx = cx
    return centres


def _speaker_centres(samples: Sequence[Tuple[float, List[Face]]], cfg,
                     energy: Optional["object"], clip_start: float
                     ) -> Optional[List[Tuple[float, Optional[float]]]]:
    """Sigue a quien mueve la boca mientras suena audio.

    El criterio de antes (la cara mas cercana al encuadre, o la mas grande)
    encuadra bien cuando hay una sola persona, pero en un plano de dos se
    queda en el que escucha durante minutos enteros.

    Tres cosas evitan que el encuadre se vuelva loco saltando de uno a otro:

      1. Solo cuenta el movimiento del TERCIO INFERIOR de la cara.
      2. Ese movimiento se promedia sobre unos segundos, no por muestra.
      3. Para cambiar de persona, la otra tiene que ganar por un margen y
         mantenerlo varias muestras seguidas.

    Devuelve None si no hay senal suficiente (una sola cara, o ningun
    movimiento medible): entonces manda el criterio de siempre.
    """
    buckets = {face.bucket for _, faces in samples for face in faces}
    if len(buckets) < 2:
        return None
    if not any(face.mouth > 0 for _, faces in samples for face in faces):
        return None            # sin medida de boca (primera muestra, o clip fijo)

    window = max(1, int(round(getattr(cfg, "talk_window", 2.5) /
                              max(cfg.sample_interval, 1e-6))))
    margin = max(1.0, getattr(cfg, "switch_margin", 1.4))
    hold = max(1, int(getattr(cfg, "switch_hold", 2)))
    # Por debajo de este nivel de audio no habla nadie: es una pausa, una
    # risa de fondo o musica. Cambiar de encuadre ahi no tiene sentido.
    silence = 0.18

    history: dict = {}                  # bucket -> ultimas medidas de boca
    current: Optional[int] = None
    contender: Optional[int] = None
    contender_streak = 0
    last_cx: Optional[float] = None

    centres: List[Tuple[float, Optional[float]]] = []
    for moment, faces in samples:
        if not faces:
            centres.append((moment, None))
            continue

        loud = True
        if energy is not None:
            loud = energy.at(clip_start + moment) >= silence

        by_bucket = {}
        for face in faces:
            # Si el detector saca dos cajas de la misma persona nos quedamos
            # con la mayor: es la cara, no un trozo suelto.
            if face.bucket not in by_bucket or face.area > by_bucket[face.bucket].area:
                by_bucket[face.bucket] = face

        for bucket, face in by_bucket.items():
            values = history.setdefault(bucket, [])
            values.append(face.mouth if loud else 0.0)
            if len(values) > window:
                del values[0]

        talk = {b: _median(v) for b, v in history.items() if b in by_bucket and v}
        if not talk:
            centres.append((moment, last_cx))
            continue

        best = max(talk, key=lambda b: talk[b])
        if current is None or current not in by_bucket:
            current, contender, contender_streak = best, None, 0
        elif best != current and loud:
            # Cambiar de persona solo si gana claramente y lo sostiene.
            if talk[best] > max(talk.get(current, 0.0), 1e-6) * margin:
                if contender == best:
                    contender_streak += 1
                else:
                    contender, contender_streak = best, 1
                if contender_streak >= hold:
                    current, contender, contender_streak = best, None, 0
            else:
                contender, contender_streak = None, 0

        last_cx = by_bucket[current].cx
        centres.append((moment, last_cx))

    changes = sum(1 for i in range(1, len(centres))
                  if centres[i][1] is not None and centres[i - 1][1] is not None
                  and abs(centres[i][1] - centres[i - 1][1]) > 0.02)
    log.info("Reencuadre: siguiendo a quien habla (%d cambio(s) de plano).",
             changes)
    return centres


def _pick(faces: Sequence[Face], previous_cx: Optional[float]) -> Optional[float]:
    """Elige a que cara seguir en este fotograma."""
    if not faces:
        return None
    if previous_cx is None or len(faces) == 1:
        return max(faces, key=lambda f: f.area).cx

    # Con varias caras en plano preferimos la mas cercana al encuadre actual:
    # saltar de invitado a invitado cada dos fotogramas marea mas que ayuda.
    # Una cara claramente mayor (primer plano) si justifica el salto.
    biggest = max(faces, key=lambda f: f.area)
    nearest = min(faces, key=lambda f: abs(f.cx - previous_cx))
    return biggest.cx if biggest.area > nearest.area * 1.8 else nearest.cx


def _fill_gaps(samples: List[Tuple[float, Optional[float]]]
               ) -> Optional[List[Tuple[float, float]]]:
    """Rellena las muestras sin deteccion manteniendo el ultimo valor valido."""
    known = [cx for _, cx in samples if cx is not None]
    if not known:
        return None

    filled: List[Tuple[float, float]] = []
    last = known[0]
    for t, cx in samples:
        if cx is not None:
            last = cx
        filled.append((t, last))
    return filled


def _smooth(points: List[Tuple[float, float]], window: float, interval: float
            ) -> List[Tuple[float, float]]:
    """Media movil, para que el encuadre se deslice en vez de dar tirones."""
    span = max(1, int(round(window / max(interval, 1e-6))))
    if span <= 1:
        return points

    values = [cx for _, cx in points]
    smoothed = []
    for i, (t, _) in enumerate(points):
        lo = max(0, i - span // 2)
        hi = min(len(values), i + span // 2 + 1)
        smoothed.append((t, sum(values[lo:hi]) / (hi - lo)))
    return smoothed


def _to_keyframes(points: List[Tuple[float, float]], deadzone: float, limit: int
                  ) -> List[Tuple[float, float]]:
    """Reduce la trayectoria a los puntos donde el encuadre cambia de verdad."""
    if not points:
        return []

    keyframes = [points[0]]
    for t, cx in points[1:]:
        if abs(cx - keyframes[-1][1]) >= deadzone:
            keyframes.append((t, cx))
    if keyframes[-1][0] < points[-1][0]:
        keyframes.append(points[-1])

    # La expresion de ffmpeg crece con el numero de tramos; pasado cierto punto
    # no aporta suavidad, solo tiempo de evaluacion por fotograma.
    if len(keyframes) > limit:
        stride = len(keyframes) / limit
        keyframes = [keyframes[int(i * stride)] for i in range(limit - 1)] + [keyframes[-1]]

    return keyframes
