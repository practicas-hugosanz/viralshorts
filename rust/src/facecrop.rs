//! Reencuadre inteligente: seguir la cara en vez de recortar por el centro.
//!
//! Un podcast se graba en 16:9 con dos o tres personas repartidas por el plano.
//! Recortar por el centro parte a los invitados por la mitad. Aqui se muestrea
//! el clip, se detectan caras y se construye una trayectoria suave del centro
//! del encuadre, que despues se traduce a una expresion de ffmpeg.
//!
//! Se usa YuNet (un detector pequeno, ~230 KB) a traves de ONNX Runtime, porque
//! aguanta caras de perfil, con gafas y mal iluminadas, que es exactamente lo
//! que hay en un podcast. Los fotogramas los decodifica ffmpeg. Si algo falla
//! (sin modelo, sin onnxruntime) se cae al recorte centrado de siempre: el
//! pipeline nunca se rompe por esto.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Stdio};
use std::sync::{Mutex, OnceLock};

use serde_json::{json, Value};
use sha1::{Digest, Sha1};

use crate::audio_energy::EnergyTrack;
use crate::config::ReframeConfig;
use crate::ffmpeg::{self, command};
use crate::words::Track;

/// Ancho al que se reduce cada fotograma antes de detectar: la deteccion no
/// necesita 1080p y a 480 va varias veces mas rapido.
const DETECT_WIDTH: usize = 480;
/// El modelo YuNet de opencv_zoo es de entrada fija 640x640.
const MODEL_SIZE: usize = 640;
const NMS_THRESHOLD: f32 = 0.3;
const TOP_K: usize = 5000;
const MODEL_FILE: &str = "face_detection_yunet_2023mar.onnx";

// ----------------------------------------------------------------------------
// Caras
// ----------------------------------------------------------------------------

/// Una cara detectada en un fotograma reducido.
#[derive(Debug, Clone, PartialEq)]
pub struct Face {
    /// Centro horizontal, normalizado 0..1.
    pub cx: f64,
    /// En pixeles del fotograma reducido.
    pub area: f64,
    /// (x, y, w, h) en el fotograma reducido.
    pub box_: (i32, i32, i32, i32),
    /// Cuanto cambia esa zona entre muestras.
    pub motion: f64,
    /// Cuanto cambia el TERCIO INFERIOR de la cara.
    pub mouth: f64,
}

impl Face {
    pub fn new(cx: f64, area: f64, box_: (i32, i32, i32, i32)) -> Self {
        Self { cx, area, box_, motion: 0.0, mouth: 0.0 }
    }

    /// Identidad aproximada: quien esta sentado en ese sitio del plano.
    pub fn bucket(&self) -> i64 {
        (self.cx / 0.06).round_ties_even() as i64
    }
}

/// Un fotograma reducido, en BGR (3 bytes por pixel).
#[derive(Debug, Clone)]
pub struct Frame {
    pub width: usize,
    pub height: usize,
    pub bgr: Vec<u8>,
}

impl Frame {
    pub fn gray(&self) -> Vec<u8> {
        self.bgr
            .chunks_exact(3)
            .map(|p| ((p[2] as u32 * 299 + p[1] as u32 * 587 + p[0] as u32 * 114 + 500) / 1000) as u8)
            .collect()
    }
}

pub trait Detector {
    fn detect(&mut self, frame: &Frame) -> Vec<Face>;
}

// ----------------------------------------------------------------------------
// Cache de trayectorias
// ----------------------------------------------------------------------------
//
// Detectar caras es lo mas lento del montaje de un clip despues de codificar. Y
// se repite entero cada vez que se vuelve a montar el mismo momento: al cambiar
// un titulo, al retocar el estilo de los subtitulos, al rehacer un short. La
// trayectoria depende solo del video, del tramo y de los ajustes de reencuadre,
// asi que se guarda.

fn cache_path(video: &Path, start: f64, duration: f64, cfg: &ReframeConfig, cache_dir: Option<&Path>) -> Option<PathBuf> {
    let cache_dir = cache_dir?;
    let meta = std::fs::metadata(video).ok()?;
    // Un video descargado se reconoce por su enlace: si se borro para no
    // ocupar disco y se ha vuelto a bajar, las caras ya estan calculadas.
    let identity = match crate::download::identity(video) {
        Some(link) => format!("enlace:{link}"),
        None => {
            let mtime = meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
            let resolved = std::fs::canonicalize(video).unwrap_or_else(|_| video.to_path_buf());
            format!("{}|{}|{mtime}", resolved.display(), meta.len())
        }
    };
    let signature = [
        // v2: guarda tambien las dos personas y quien habla (pantalla partida).
        "yunet-ort-2".to_string(),
        identity,
        format!("{:.2}", start),
        format!("{:.2}", duration),
        cfg.sample_interval.to_string(),
        cfg.smoothing_window.to_string(),
        cfg.deadzone.to_string(),
        cfg.max_keyframes.to_string(),
        cfg.ignore_static_faces.to_string(),
        cfg.min_face_motion.to_string(),
        cfg.static_ratio.to_string(),
        cfg.score_threshold.to_string(),
        cfg.speaker_tracking.to_string(),
        cfg.talk_window.to_string(),
        cfg.switch_margin.to_string(),
        cfg.switch_hold.to_string(),
    ]
    .join("|");
    Some(cache_dir.join(format!("reframe_{}.json", &format!("{:x}", Sha1::digest(signature.as_bytes()))[..16])))
}

fn read_cache(path: Option<&Path>) -> Option<Track> {
    let data: Value = serde_json::from_str(&std::fs::read_to_string(path?).ok()?).ok()?;
    let points = data["puntos"].as_array()?.iter().map(|p| Some((p[0].as_f64()?, p[1].as_f64()?))).collect::<Option<Vec<_>>>()?;
    let mut track = Track::new(points, data["deteccion"].as_f64()?);
    track.duo = data["duo"].as_array().and_then(|d| Some((d.first()?.as_f64()?, d.get(1)?.as_f64()?)));
    track.speakers = data["hablan"]
        .as_array()
        .map(|list| list.iter().filter_map(|p| Some((p[0].as_f64()?, p[1].as_u64()? as u8))).collect())
        .unwrap_or_default();
    Some(track)
}

/// Guarda la trayectoria. Un clip sin caras se guarda como lista vacia: tambien
/// cuesta minutos averiguarlo y tampoco hay que repetirlo.
fn write_cache(path: Option<&Path>, track: Option<&Track>) {
    let Some(path) = path else { return };
    let data = match track {
        None => json!({"puntos": [], "deteccion": 0.0}),
        Some(t) => json!({
            "puntos": t.points.iter().map(|(time, cx)| json!([(time * 1000.0).round() / 1000.0, (cx * 100000.0).round() / 100000.0])).collect::<Vec<_>>(),
            "deteccion": (t.detection_ratio * 10000.0).round() / 10000.0,
            "duo": t.duo.map(|(a, b)| json!([a, b])),
            "hablan": t.speakers.iter().map(|(time, who)| json!([(time * 1000.0).round() / 1000.0, who])).collect::<Vec<_>>(),
        }),
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(error) = std::fs::write(path, data.to_string()) {
        log::debug!("No se pudo guardar el reencuadre en cache: {error}");
    }
}

// ----------------------------------------------------------------------------
// Punto de entrada
// ----------------------------------------------------------------------------

/// Trayectoria del encuadre para [start, start+duration).
///
/// `energy` es la curva de energia del audio del podcast entero (opcional). Con
/// ella el seguimiento distingue al que habla del que escucha: cuando no suena
/// nada, el encuadre se queda donde esta en vez de irse detras del primero que
/// se rasque la nariz.
pub fn track_faces(video: &Path, start: f64, duration: f64, cfg: &ReframeConfig, cache_dir: Option<&Path>, energy: Option<&EnergyTrack>) -> Option<Track> {
    if !cfg.enabled {
        return None;
    }

    let cache = cache_path(video, start, duration, cfg, cache_dir);
    if let Some(cached) = read_cache(cache.as_deref()) {
        log::info!("Reencuadre en cache ({} puntos).", cached.points.len());
        return if cached.points.is_empty() { None } else { Some(cached) };
    }

    let info = match ffmpeg::probe(video) {
        Ok(info) => info,
        Err(error) => {
            log::warn!("No se pudo leer {} para el reencuadre ({error}); se usara recorte centrado.", video.display());
            return None;
        }
    };
    let (det_w, det_h) = detection_size(info.width.max(1) as usize, info.height.max(1) as usize);

    let mut detector = match YuNet::open(cfg, cache_dir) {
        Ok(detector) => detector,
        Err(error) => {
            log::warn!("No hay detector de caras ({error}); se usara recorte centrado.");
            return None;
        }
    };
    let frames = match FrameReader::open(video, start, duration, cfg.sample_interval, det_w, det_h) {
        Ok(frames) => frames,
        Err(error) => {
            log::warn!("No se pudo leer el video para el reencuadre ({error}); se usara recorte centrado.");
            return None;
        }
    };

    let samples = sample(frames, &mut detector, cfg.sample_interval);
    track_from_samples(samples, cfg, energy, start, cache.as_deref())
}

/// Todo lo que viene despues de detectar: sin caras reales no hay nada que
/// medir, pero esto se puede probar con caras inventadas.
pub fn track_from_samples(
    samples: Vec<(f64, Vec<Face>)>,
    cfg: &ReframeConfig,
    energy: Option<&EnergyTrack>,
    clip_start: f64,
    cache: Option<&Path>,
) -> Option<Track> {
    if samples.is_empty() {
        log::info!("No se detecto ninguna cara; se usara recorte centrado.");
        write_cache(cache, None);
        return None;
    }

    // La pantalla partida se busca ANTES de quitar las caras quietas: quien
    // escucha se mueve menos que quien habla y el filtro se lo llevaria.
    let duo = duo_positions(&samples).filter(|&(left, right)| both_alive(&samples, left, right, cfg));
    let speakers = duo
        .and_then(|(left, right)| speaker_centres(&samples, cfg, energy, clip_start).map(|talk| speaker_timeline(&talk, left, right)))
        .unwrap_or_default();
    let samples = drop_static_faces(samples, cfg);
    let (centres, _) = choose_centres_detail(&samples, cfg, energy, clip_start);

    let ratio = centres.iter().filter(|(_, cx)| cx.is_some()).count() as f64 / centres.len() as f64;
    let Some(filled) = fill_gaps(&centres) else {
        log::info!("No se detecto ninguna cara; se usara recorte centrado.");
        write_cache(cache, None);
        return None;
    };

    let smoothed = smooth(&filled, cfg.smoothing_window, cfg.sample_interval);
    let mut track = Track::new(to_keyframes(&smoothed, cfg.deadzone, cfg.max_keyframes.max(1) as usize), ratio);
    track.duo = duo;
    track.speakers = speakers;

    if !track.is_useful() {
        log::info!("Deteccion poco fiable ({:.0}% de las muestras); se usara recorte centrado.", ratio * 100.0);
        write_cache(cache, None);
        return None;
    }

    log::info!("Reencuadre: cara detectada en el {:.0}% de las muestras, {} puntos de trayectoria.", ratio * 100.0, track.points.len());
    write_cache(cache, Some(&track));
    Some(track)
}

/// Tamano del fotograma de deteccion: 480 de ancho, y sin pasarse de lo que
/// cabe en el modelo.
pub fn detection_size(width: usize, height: usize) -> (usize, usize) {
    let mut scale = if width > DETECT_WIDTH { DETECT_WIDTH as f64 / width as f64 } else { 1.0 };
    if (height as f64 * scale) as usize > MODEL_SIZE {
        scale = MODEL_SIZE as f64 / height as f64;
    }
    if scale >= 1.0 {
        return (width.min(MODEL_SIZE), height.min(MODEL_SIZE));
    }
    (((width as f64 * scale) as usize).max(2), ((height as f64 * scale) as usize).max(2))
}

// ----------------------------------------------------------------------------
// Muestreo
// ----------------------------------------------------------------------------

/// Muestrea el clip y devuelve TODAS las caras de cada muestra.
///
/// A cada cara se le mide ademas cuanto cambia su zona respecto a la muestra
/// anterior. Esa es la unica diferencia observable entre una persona hablando y
/// la foto de una persona pegada en la pared o metida en el montaje: la persona
/// se mueve, la foto no.
pub fn sample(frames: impl Iterator<Item = Frame>, detector: &mut dyn Detector, interval: f64) -> Vec<(f64, Vec<Face>)> {
    let mut samples = Vec::new();
    let mut previous: Option<(usize, usize, Vec<u8>)> = None;

    for (index, frame) in frames.enumerate() {
        let gray = frame.gray();
        let mut faces = detector.detect(&frame);
        if let Some((w, h, before)) = &previous {
            if (*w, *h) == (frame.width, frame.height) {
                for face in &mut faces {
                    face.motion = region_motion(before, &gray, frame.width, frame.height, face.box_);
                    face.mouth = region_motion(before, &gray, frame.width, frame.height, mouth_box(face.box_));
                }
            }
        }
        samples.push((index as f64 * interval, faces));
        previous = Some((frame.width, frame.height, gray));
    }
    samples
}

/// Tercio inferior de la cara: donde se nota si esa persona esta hablando. La
/// cara entera se mueve tambien cuando alguien asiente o se rie de lo que dice
/// otro. La boca solo se mueve cuando habla.
pub fn mouth_box(box_: (i32, i32, i32, i32)) -> (i32, i32, i32, i32) {
    let (x, y, w, h) = box_;
    (x + w / 6, y + (h as f64 * 0.60) as i32, 2.max(w - w / 3), 2.max((h as f64 * 0.40) as i32))
}

/// Cambio medio de luminosidad dentro de la caja, en niveles de gris.
pub fn region_motion(before: &[u8], after: &[u8], width: usize, height: usize, box_: (i32, i32, i32, i32)) -> f64 {
    let (x, y, w, h) = box_;
    let (x0, y0) = (x.max(0), y.max(0));
    let (x1, y1) = ((width as i32).min(x + w), (height as i32).min(y + h));
    if x1 - x0 < 4 || y1 - y0 < 4 {
        return 0.0;
    }
    let mut total = 0u64;
    for row in y0..y1 {
        let base = row as usize * width;
        for col in x0..x1 {
            total += (after[base + col as usize] as i32 - before[base + col as usize] as i32).unsigned_abs() as u64;
        }
    }
    total as f64 / ((x1 - x0) as usize * (y1 - y0) as usize) as f64
}

/// Quita las caras que en realidad son fotos, carteles o miniaturas.
///
/// Se agrupan las detecciones por posicion horizontal y se mira el movimiento
/// TIPICO de cada grupo a lo largo del clip, no el de un fotograma suelto: un
/// invitado callado tambien se queda quieto un segundo, pero no un minuto.
///
/// Si todas las caras del clip estan quietas (el video corta a una foto a
/// pantalla completa, por ejemplo) no se descarta ninguna: es mejor encuadrar la
/// foto que perder el reencuadre entero.
pub fn drop_static_faces(samples: Vec<(f64, Vec<Face>)>, cfg: &ReframeConfig) -> Vec<(f64, Vec<Face>)> {
    if !cfg.ignore_static_faces {
        return samples;
    }

    let mut buckets: HashMap<i64, Vec<f64>> = HashMap::new();
    for (_, faces) in &samples {
        for face in faces {
            buckets.entry(face.bucket()).or_default().push(face.motion);
        }
    }

    // Un grupo con dos apariciones no dice nada; hace falta presencia sostenida.
    let steady: HashMap<i64, f64> = buckets.iter().filter(|(_, v)| v.len() >= 4).map(|(&k, v)| (k, median(v))).collect();
    if steady.is_empty() {
        return samples;
    }

    let liveliest = steady.values().cloned().fold(f64::NEG_INFINITY, f64::max);
    if liveliest < cfg.min_face_motion {
        return samples; // aqui no se mueve nada: no hay con que comparar
    }

    let cut = cfg.min_face_motion.max(liveliest * cfg.static_ratio);
    let dead: Vec<i64> = steady.iter().filter(|(_, &value)| value < cut).map(|(&k, _)| k).collect();
    if dead.is_empty() {
        return samples;
    }

    log::info!("Reencuadre: {} cara(s) descartadas por estar inmoviles (fotos o carteles, no gente hablando).", dead.len());
    samples
        .into_iter()
        .map(|(t, faces)| (t, faces.into_iter().filter(|f| !dead.contains(&f.bucket())).collect()))
        .collect()
}

pub fn median(values: &[f64]) -> f64 {
    let mut ordered = values.to_vec();
    ordered.sort_by(|a, b| a.total_cmp(b));
    let middle = ordered.len() / 2;
    if ordered.len() % 2 == 1 { ordered[middle] } else { (ordered[middle - 1] + ordered[middle]) / 2.0 }
}

/// De todas las caras de cada muestra, a cual se sigue.
pub fn choose_centres(samples: &[(f64, Vec<Face>)], cfg: &ReframeConfig, energy: Option<&EnergyTrack>, clip_start: f64) -> Vec<(f64, Option<f64>)> {
    choose_centres_detail(samples, cfg, energy, clip_start).0
}

/// Como `choose_centres`, y dice si los centros siguen a quien habla (true) o
/// solo a la cara mas a mano (false).
fn choose_centres_detail(samples: &[(f64, Vec<Face>)], cfg: &ReframeConfig, energy: Option<&EnergyTrack>, clip_start: f64) -> (Vec<(f64, Option<f64>)>, bool) {
    if cfg.speaker_tracking {
        if let Some(speaking) = speaker_centres(samples, cfg, energy, clip_start) {
            return (speaking, true);
        }
    }

    let mut centres = Vec::with_capacity(samples.len());
    let mut previous: Option<f64> = None;
    for (moment, faces) in samples {
        let cx = pick(faces, previous);
        centres.push((*moment, cx));
        if cx.is_some() {
            previous = cx;
        }
    }
    (centres, false)
}

/// Dos personas en plano A LA VEZ: el centro horizontal de cada una
/// (izquierda, derecha). Se agrupan los centros de todas las caras en dos
/// grupos; valen si estan bien separados y aparecen juntos en buena parte del
/// clip. None con una sola persona o con un video que cambia de camara (en
/// cada plano hay una cara, nunca las dos).
pub fn duo_positions(samples: &[(f64, Vec<Face>)]) -> Option<(f64, f64)> {
    let with_faces: Vec<&Vec<Face>> = samples.iter().map(|(_, faces)| faces).filter(|faces| !faces.is_empty()).collect();
    if with_faces.len() < 3 {
        return None;
    }
    let all: Vec<f64> = with_faces.iter().flat_map(|faces| faces.iter().map(|f| f.cx)).collect();
    // k-medias con k=2 en una dimension: pocas vueltas bastan.
    let (mut left, mut right) = (all.iter().copied().fold(f64::MAX, f64::min), all.iter().copied().fold(f64::MIN, f64::max));
    for _ in 0..12 {
        let middle = (left + right) / 2.0;
        let (l, r): (Vec<f64>, Vec<f64>) = all.iter().partition(|cx| **cx < middle);
        if l.is_empty() || r.is_empty() {
            return None;
        }
        left = median(&l);
        right = median(&r);
    }
    if right - left < 0.2 {
        return None;
    }
    let middle = (left + right) / 2.0;
    let together = with_faces.iter().filter(|faces| faces.iter().any(|f| f.cx < middle) && faces.iter().any(|f| f.cx >= middle)).count();
    (together as f64 >= with_faces.len() as f64 * 0.4).then_some((left, right))
}

/// Las dos personas de la pantalla partida se mueven (aunque una escuche): un
/// cartel o una foto no. Sin medida de movimiento (todo a cero) no se descarta.
fn both_alive(samples: &[(f64, Vec<Face>)], left: f64, right: f64, cfg: &ReframeConfig) -> bool {
    let middle = (left + right) / 2.0;
    let side = |is_left: bool| -> Vec<f64> {
        samples.iter().flat_map(|(_, faces)| faces.iter().filter(move |f| (f.cx < middle) == is_left).map(|f| f.motion)).collect()
    };
    let (a, b) = (side(true), side(false));
    if a.iter().chain(&b).all(|m| *m == 0.0) {
        return true;
    }
    let floor = cfg.min_face_motion * 0.5;
    median(&a) >= floor && median(&b) >= floor
}

/// Quien habla en cada momento (0 izquierda, 1 derecha) a partir de los
/// centros que siguen a quien habla. Solo se apuntan los cambios.
pub fn speaker_timeline(centres: &[(f64, Option<f64>)], left: f64, right: f64) -> Vec<(f64, u8)> {
    let mut timeline: Vec<(f64, u8)> = Vec::new();
    for (moment, cx) in centres {
        let Some(cx) = cx else { continue };
        let who = u8::from((cx - right).abs() < (cx - left).abs());
        if timeline.last().is_none_or(|(_, last)| *last != who) {
            timeline.push((*moment, who));
        }
    }
    timeline
}

/// Sigue a quien mueve la boca mientras suena audio.
///
/// El criterio de antes (la cara mas cercana al encuadre, o la mas grande)
/// encuadra bien cuando hay una sola persona, pero en un plano de dos se queda
/// en el que escucha durante minutos enteros.
///
/// Tres cosas evitan que el encuadre se vuelva loco saltando de uno a otro:
///
///   1. Solo cuenta el movimiento del TERCIO INFERIOR de la cara.
///   2. Ese movimiento se promedia sobre unos segundos, no por muestra.
///   3. Para cambiar de persona, la otra tiene que ganar por un margen y
///      mantenerlo varias muestras seguidas.
///
/// Devuelve None si no hay senal suficiente (una sola cara, o ningun movimiento
/// medible): entonces manda el criterio de siempre.
pub fn speaker_centres(samples: &[(f64, Vec<Face>)], cfg: &ReframeConfig, energy: Option<&EnergyTrack>, clip_start: f64) -> Option<Vec<(f64, Option<f64>)>> {
    let mut seen: Vec<i64> = Vec::new();
    for (_, faces) in samples {
        for face in faces {
            if !seen.contains(&face.bucket()) {
                seen.push(face.bucket());
            }
        }
    }
    if seen.len() < 2 {
        return None;
    }
    if !samples.iter().any(|(_, faces)| faces.iter().any(|f| f.mouth > 0.0)) {
        return None; // sin medida de boca (primera muestra, o clip fijo)
    }

    let window = ((cfg.talk_window / cfg.sample_interval.max(1e-6)).round_ties_even() as i64).max(1) as usize;
    let margin = cfg.switch_margin.max(1.0);
    let hold = cfg.switch_hold.max(1) as usize;
    // Por debajo de este nivel de audio no habla nadie: es una pausa, una risa
    // de fondo o musica. Cambiar de encuadre ahi no tiene sentido.
    let silence = 0.18;

    let mut history: Vec<(i64, Vec<f64>)> = Vec::new(); // en orden de aparicion
    let mut current: Option<i64> = None;
    let mut contender: Option<i64> = None;
    let mut contender_streak = 0usize;
    let mut last_cx: Option<f64> = None;
    let mut centres: Vec<(f64, Option<f64>)> = Vec::with_capacity(samples.len());

    for (moment, faces) in samples {
        if faces.is_empty() {
            centres.push((*moment, None));
            continue;
        }

        let loud = energy.is_none_or(|e| e.at(clip_start + moment) >= silence);

        // Si el detector saca dos cajas de la misma persona se queda la mayor:
        // es la cara, no un trozo suelto.
        let mut by_bucket: Vec<(i64, &Face)> = Vec::new();
        for face in faces {
            match by_bucket.iter_mut().find(|(b, _)| *b == face.bucket()) {
                Some(entry) => {
                    if face.area > entry.1.area {
                        entry.1 = face;
                    }
                }
                None => by_bucket.push((face.bucket(), face)),
            }
        }

        for (bucket, face) in &by_bucket {
            let values = match history.iter_mut().find(|(b, _)| b == bucket) {
                Some(entry) => &mut entry.1,
                None => {
                    history.push((*bucket, Vec::new()));
                    &mut history.last_mut().expect("recien anadido").1
                }
            };
            values.push(if loud { face.mouth } else { 0.0 });
            if values.len() > window {
                values.remove(0);
            }
        }

        let talk: Vec<(i64, f64)> =
            history.iter().filter(|(b, v)| by_bucket.iter().any(|(x, _)| x == b) && !v.is_empty()).map(|(b, v)| (*b, median(v))).collect();
        if talk.is_empty() {
            centres.push((*moment, last_cx));
            continue;
        }

        let mut best = talk[0];
        for &entry in &talk[1..] {
            if entry.1 > best.1 {
                best = entry;
            }
        }
        let talk_of = |bucket: i64| talk.iter().find(|(b, _)| *b == bucket).map(|(_, v)| *v);

        match current.filter(|c| by_bucket.iter().any(|(b, _)| b == c)) {
            None => {
                current = Some(best.0);
                contender = None;
                contender_streak = 0;
            }
            Some(now) if best.0 != now && loud => {
                // Cambiar de persona solo si gana claramente y lo sostiene.
                if best.1 > talk_of(now).unwrap_or(0.0).max(1e-6) * margin {
                    if contender == Some(best.0) {
                        contender_streak += 1;
                    } else {
                        contender = Some(best.0);
                        contender_streak = 1;
                    }
                    if contender_streak >= hold {
                        current = Some(best.0);
                        contender = None;
                        contender_streak = 0;
                    }
                } else {
                    contender = None;
                    contender_streak = 0;
                }
            }
            Some(_) => {}
        }

        let chosen = current.expect("current queda fijado arriba");
        last_cx = by_bucket.iter().find(|(b, _)| *b == chosen).map(|(_, f)| f.cx);
        centres.push((*moment, last_cx));
    }

    let changes = centres
        .windows(2)
        .filter(|w| matches!((w[0].1, w[1].1), (Some(a), Some(b)) if (b - a).abs() > 0.02))
        .count();
    log::info!("Reencuadre: siguiendo a quien habla ({changes} cambio(s) de plano).");
    Some(centres)
}

/// Elige a que cara seguir en este fotograma.
pub fn pick(faces: &[Face], previous_cx: Option<f64>) -> Option<f64> {
    if faces.is_empty() {
        return None;
    }
    let biggest = first_by(faces, |f| -f.area);
    let Some(previous) = previous_cx.filter(|_| faces.len() > 1) else { return Some(biggest.cx) };

    // Con varias caras en plano se prefiere la mas cercana al encuadre actual:
    // saltar de invitado a invitado cada dos fotogramas marea mas que ayuda. Una
    // cara claramente mayor (primer plano) si justifica el salto.
    let nearest = first_by(faces, |f| (f.cx - previous).abs());
    Some(if biggest.area > nearest.area * 1.8 { biggest.cx } else { nearest.cx })
}

/// El primero con la clave minima (como `min`/`max` de Python en un empate).
fn first_by(faces: &[Face], key: impl Fn(&Face) -> f64) -> &Face {
    let mut best = &faces[0];
    for face in &faces[1..] {
        if key(face) < key(best) {
            best = face;
        }
    }
    best
}

/// Rellena las muestras sin deteccion manteniendo el ultimo valor valido.
pub fn fill_gaps(samples: &[(f64, Option<f64>)]) -> Option<Vec<(f64, f64)>> {
    let first = samples.iter().find_map(|(_, cx)| *cx)?;
    let mut last = first;
    Some(
        samples
            .iter()
            .map(|&(t, cx)| {
                if let Some(cx) = cx {
                    last = cx;
                }
                (t, last)
            })
            .collect(),
    )
}

/// Media movil, para que el encuadre se deslice en vez de dar tirones.
pub fn smooth(points: &[(f64, f64)], window: f64, interval: f64) -> Vec<(f64, f64)> {
    let span = ((window / interval.max(1e-6)).round_ties_even() as i64).max(1) as usize;
    if span <= 1 {
        return points.to_vec();
    }
    let values: Vec<f64> = points.iter().map(|(_, cx)| *cx).collect();
    points
        .iter()
        .enumerate()
        .map(|(i, &(t, _))| {
            let lo = i.saturating_sub(span / 2);
            let hi = (i + span / 2 + 1).min(values.len());
            (t, values[lo..hi].iter().sum::<f64>() / (hi - lo) as f64)
        })
        .collect()
}

/// Reduce la trayectoria a los puntos donde el encuadre cambia de verdad.
pub fn to_keyframes(points: &[(f64, f64)], deadzone: f64, limit: usize) -> Vec<(f64, f64)> {
    let Some(&first) = points.first() else { return Vec::new() };
    let mut keyframes = vec![first];
    for &(t, cx) in &points[1..] {
        if (cx - keyframes[keyframes.len() - 1].1).abs() >= deadzone {
            keyframes.push((t, cx));
        }
    }
    let last = points[points.len() - 1];
    if keyframes[keyframes.len() - 1].0 < last.0 {
        keyframes.push(last);
    }

    // La expresion de ffmpeg crece con el numero de tramos; pasado cierto punto
    // no aporta suavidad, solo tiempo de evaluacion por fotograma.
    if keyframes.len() > limit {
        let stride = keyframes.len() as f64 / limit as f64;
        let mut reduced: Vec<(f64, f64)> = (0..limit.saturating_sub(1)).map(|i| keyframes[(i as f64 * stride) as usize]).collect();
        reduced.push(keyframes[keyframes.len() - 1]);
        keyframes = reduced;
    }
    keyframes
}

// ----------------------------------------------------------------------------
// Fotogramas con ffmpeg
// ----------------------------------------------------------------------------

/// Lee un fotograma cada `interval` segundos del tramo, ya reducido y en BGR.
pub struct FrameReader {
    child: Child,
    stdout: ChildStdout,
    width: usize,
    height: usize,
}

impl FrameReader {
    pub fn open(video: &Path, start: f64, duration: f64, interval: f64, width: usize, height: usize) -> Result<Self, String> {
        let filter = format!("fps=1/{},scale={width}:{height}:flags=bilinear", interval.max(0.05));
        let mut child = command("ffmpeg")
            .args(["-v", "error", "-nostdin", "-ss", &format!("{start:.3}"), "-t", &format!("{duration:.3}"), "-i"])
            .arg(video)
            .args(["-an", "-sn", "-vf", &filter, "-f", "rawvideo", "-pix_fmt", "bgr24", "-"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("no se pudo lanzar ffmpeg: {e}"))?;
        let stdout = child.stdout.take().ok_or("sin salida de ffmpeg")?;
        Ok(Self { child, stdout, width, height })
    }
}

impl Iterator for FrameReader {
    type Item = Frame;

    fn next(&mut self) -> Option<Frame> {
        let mut bgr = vec![0u8; self.width * self.height * 3];
        let mut filled = 0;
        while filled < bgr.len() {
            match self.stdout.read(&mut bgr[filled..]) {
                Ok(0) => return None, // un fotograma a medias no sirve
                Ok(n) => filled += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => return None,
            }
        }
        Some(Frame { width: self.width, height: self.height, bgr })
    }
}

impl Drop for FrameReader {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ----------------------------------------------------------------------------
// YuNet con ONNX Runtime
// ----------------------------------------------------------------------------

/// Una sola descarga del modelo aunque haya varios renders en marcha.
static MODEL_LOCK: Mutex<()> = Mutex::new(());

/// Una sola sesion de onnxruntime para todo el proceso: cargar el modelo cuesta
/// y varios renders a la vez se turnan para usarla.
static SESSION: OnceLock<Result<Mutex<ort::session::Session>, String>> = OnceLock::new();

pub struct YuNet {
    threshold: f32,
}

impl YuNet {
    pub fn open(cfg: &ReframeConfig, cache_dir: Option<&Path>) -> Result<Self, String> {
        let model = ensure_model(cfg, cache_dir)?;
        let session = SESSION.get_or_init(|| {
            let load = || -> Result<ort::session::Session, ort::Error> {
                let env = ort::init().build()?;
                ort::session::Session::builder(&env)?.with_intra_threads(2)?.commit_from_file(&model)
            };
            load().map(Mutex::new).map_err(|e| format!("no se pudo iniciar YuNet: {e}"))
        });
        match session {
            Ok(_) => Ok(Self { threshold: cfg.score_threshold as f32 }),
            Err(error) => Err(error.clone()),
        }
    }

    fn run(&self, frame: &Frame) -> Result<Vec<RawBox>, ort::Error> {
        let (w, h) = (frame.width.min(MODEL_SIZE), frame.height.min(MODEL_SIZE));
        // Entrada NCHW en BGR con valores 0..255, rellena con ceros por la
        // derecha y por abajo hasta los 640x640 del modelo: asi las cajas
        // salen en las coordenadas del fotograma, sin reescalar.
        let plane = MODEL_SIZE * MODEL_SIZE;
        let mut input = vec![0f32; 3 * plane];
        for y in 0..h {
            for x in 0..w {
                let pixel = (y * frame.width + x) * 3;
                let at = y * MODEL_SIZE + x;
                input[at] = frame.bgr[pixel] as f32;
                input[plane + at] = frame.bgr[pixel + 1] as f32;
                input[2 * plane + at] = frame.bgr[pixel + 2] as f32;
            }
        }
        let tensor = ort::value::Tensor::from_array(([1usize, 3, MODEL_SIZE, MODEL_SIZE], input))?;

        let guard = SESSION.get().and_then(|s| s.as_ref().ok()).expect("open() inicia la sesion");
        let mut session = guard.lock().expect("sesion de onnxruntime");
        let outputs = session.run(ort::inputs!["input" => tensor])?;

        let get = |name: &str| -> Result<Vec<f32>, ort::Error> { Ok(outputs[name].try_extract_tensor::<f32>()?.1.to_vec()) };
        let maps = Maps {
            cls: [get("cls_8")?, get("cls_16")?, get("cls_32")?],
            obj: [get("obj_8")?, get("obj_16")?, get("obj_32")?],
            bbox: [get("bbox_8")?, get("bbox_16")?, get("bbox_32")?],
        };
        Ok(decode(&maps, self.threshold, NMS_THRESHOLD))
    }
}

impl Detector for YuNet {
    fn detect(&mut self, frame: &Frame) -> Vec<Face> {
        match self.run(frame) {
            Ok(boxes) => boxes
                .into_iter()
                .map(|b| Face::new((b.x as f64 + b.w as f64 / 2.0) / frame.width as f64, b.w as f64 * b.h as f64, (b.x as i32, b.y as i32, b.w as i32, b.h as i32)))
                .collect(),
            Err(error) => {
                log::debug!("YuNet fallo en un fotograma: {error}");
                Vec::new()
            }
        }
    }
}

/// Ruta local al modelo YuNet, descargandolo la primera vez. Con varios clips
/// montandose a la vez esto se llama desde varios hilos: sin el cerrojo, dos
/// descargas escriben en el mismo fichero y el modelo que queda no lo puede
/// abrir nadie.
fn ensure_model(cfg: &ReframeConfig, cache_dir: Option<&Path>) -> Result<PathBuf, String> {
    if let Some(path) = cfg.model_path.as_deref().filter(|p| !p.is_empty()) {
        if Path::new(path).is_file() {
            return Ok(PathBuf::from(path));
        }
        log::warn!("reframe.model_path apunta a {path}, que no existe.");
    }

    let usable = |p: &Path| std::fs::metadata(p).is_ok_and(|m| m.len() > 10_000);
    let installed = ffmpeg::project_root().join("bin").join("models").join(MODEL_FILE);
    if usable(&installed) {
        return Ok(installed);
    }

    let target = cache_dir.unwrap_or(Path::new("work/cache")).join("face_detection_yunet.onnx");
    if usable(&target) {
        return Ok(target);
    }

    let _guard = MODEL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if usable(&target) {
        return Ok(target); // lo ha bajado el otro hilo mientras
    }
    log::info!("Descargando el modelo de deteccion de caras (una sola vez, ~230 KB)...");
    crate::fetch::download_file(&cfg.model_url, &target, "el modelo de caras", "modelo").map_err(|e| e.to_string())?;
    Ok(target)
}

// -- decodificacion de la salida (independiente de onnxruntime) -------------------

/// Las nueve salidas del modelo que hacen falta: puntuacion de clase, de
/// "objeto" y caja, a pasos de 8, 16 y 32 pixeles.
pub struct Maps {
    pub cls: [Vec<f32>; 3],
    pub obj: [Vec<f32>; 3],
    pub bbox: [Vec<f32>; 3],
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RawBox {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub score: f32,
}

/// Misma decodificacion que `FaceDetectorYN` de OpenCV: por cada celda de cada
/// rejilla, puntuacion = raiz(clase * objeto) y caja = (celda + desplazamiento)
/// * paso, con el tamano en exponencial. Despues, supresion de no maximos.
pub fn decode(maps: &Maps, score_threshold: f32, nms_threshold: f32) -> Vec<RawBox> {
    let mut boxes = Vec::new();
    for (i, stride) in [8usize, 16, 32].into_iter().enumerate() {
        let cols = MODEL_SIZE / stride;
        let rows = MODEL_SIZE / stride;
        for r in 0..rows {
            for c in 0..cols {
                let idx = r * cols + c;
                let (Some(&cls), Some(&obj)) = (maps.cls[i].get(idx), maps.obj[i].get(idx)) else { continue };
                let score = (cls.clamp(0.0, 1.0) * obj.clamp(0.0, 1.0)).sqrt();
                if score <= score_threshold {
                    continue;
                }
                let Some(b) = maps.bbox[i].get(idx * 4..idx * 4 + 4) else { continue };
                let s = stride as f32;
                let (cx, cy) = ((c as f32 + b[0]) * s, (r as f32 + b[1]) * s);
                let (w, h) = (b[2].exp() * s, b[3].exp() * s);
                boxes.push(RawBox { x: cx - w / 2.0, y: cy - h / 2.0, w, h, score });
            }
        }
    }
    nms(boxes, nms_threshold)
}

/// Supresion de no maximos: de cada grupo de cajas solapadas, la de mayor nota.
pub fn nms(mut boxes: Vec<RawBox>, threshold: f32) -> Vec<RawBox> {
    boxes.sort_by(|a, b| b.score.total_cmp(&a.score));
    boxes.truncate(TOP_K);
    let mut kept: Vec<RawBox> = Vec::new();
    for candidate in boxes {
        if kept.iter().all(|k| iou(k, &candidate) <= threshold) {
            kept.push(candidate);
        }
    }
    kept
}

fn iou(a: &RawBox, b: &RawBox) -> f32 {
    let (x1, y1) = (a.x.max(b.x), a.y.max(b.y));
    let (x2, y2) = ((a.x + a.w).min(b.x + b.w), (a.y + a.h).min(b.y + b.h));
    let inter = (x2 - x1).max(0.0) * (y2 - y1).max(0.0);
    let union = a.w * a.h + b.w * b.h - inter;
    if union <= 0.0 { 0.0 } else { inter / union }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> ReframeConfig {
        ReframeConfig::default()
    }

    /// Una cara a la altura `cx` con una caja de 60x60 a 480 de ancho.
    fn cara(cx: f64, boca: f64, movimiento: f64) -> Face {
        Face { cx, area: 3600.0, box_: ((cx * 480.0) as i32 - 30, 100, 60, 60), motion: movimiento, mouth: boca }
    }

    // -- dos personas ----------------------------------------------------------

    #[test]
    fn dos_personas_a_la_vez_dan_pantalla_partida() {
        // Plano abierto de dos: una a 0.3 y otra a 0.72, con algo de baile.
        let muestras: Vec<(f64, Vec<Face>)> =
            (0..20).map(|i| (i as f64 * 0.5, vec![cara(0.30 + (i % 3) as f64 * 0.01, 0.0, 0.0), cara(0.72 - (i % 2) as f64 * 0.01, 0.0, 0.0)])).collect();
        let (izquierda, derecha) = duo_positions(&muestras).expect("hay dos personas");
        assert!((izquierda - 0.31).abs() < 0.02 && (derecha - 0.715).abs() < 0.02, "{izquierda} {derecha}");
    }

    #[test]
    fn una_persona_o_cambios_de_camara_no_son_pantalla_partida() {
        let sola: Vec<(f64, Vec<Face>)> = (0..10).map(|i| (i as f64, vec![cara(0.5, 0.0, 0.0)])).collect();
        assert_eq!(duo_positions(&sola), None);
        // Cada plano tiene una sola cara (camaras alternas): nunca estan juntas.
        let alternas: Vec<(f64, Vec<Face>)> = (0..10).map(|i| (i as f64, vec![cara(if i % 2 == 0 { 0.3 } else { 0.7 }, 0.0, 0.0)])).collect();
        assert_eq!(duo_positions(&alternas), None);
        // Dos caras pegadas (una sola persona detectada dos veces) tampoco.
        let pegadas: Vec<(f64, Vec<Face>)> = (0..10).map(|i| (i as f64, vec![cara(0.45, 0.0, 0.0), cara(0.55, 0.0, 0.0)])).collect();
        assert_eq!(duo_positions(&pegadas), None);
    }

    #[test]
    fn un_cartel_quieto_no_es_la_segunda_persona() {
        let cfg = cfg();
        let persona = |i: usize| cara(0.3, 0.0, 3.0 + (i % 3) as f64);
        let con_cartel: Vec<(f64, Vec<Face>)> = (0..12).map(|i| (i as f64, vec![persona(i), cara(0.75, 0.0, 0.1)])).collect();
        let duo = duo_positions(&con_cartel).unwrap();
        assert!(!both_alive(&con_cartel, duo.0, duo.1, &cfg), "el cartel no se mueve");
        // Quien escucha se mueve poco, pero se mueve.
        let dos: Vec<(f64, Vec<Face>)> = (0..12).map(|i| (i as f64, vec![persona(i), cara(0.75, 0.0, 1.0)])).collect();
        let duo = duo_positions(&dos).unwrap();
        assert!(both_alive(&dos, duo.0, duo.1, &cfg));
    }

    #[test]
    fn quien_habla_se_apunta_solo_cuando_cambia() {
        let centros = [(0.0, Some(0.3)), (0.5, Some(0.31)), (1.0, None), (1.5, Some(0.7)), (2.0, Some(0.69)), (2.5, Some(0.3))];
        assert_eq!(speaker_timeline(&centros, 0.3, 0.7), [(0.0, 0), (1.5, 1), (2.5, 0)]);
        let pista = Track { speakers: speaker_timeline(&centros, 0.3, 0.7), ..Track::new(vec![], 1.0) };
        assert_eq!((pista.speaker_at(0.2), pista.speaker_at(1.6), pista.speaker_at(9.0)), (Some(0), Some(1), Some(0)));
    }

    #[test]
    fn la_cache_guarda_las_dos_personas_y_quien_habla() {
        let dir = tempfile::tempdir().unwrap();
        let ruta = dir.path().join("r.json");
        let mut pista = Track::new(vec![(0.0, 0.3), (2.0, 0.7)], 0.9);
        pista.duo = Some((0.3, 0.7));
        pista.speakers = vec![(0.0, 0), (1.5, 1)];
        write_cache(Some(&ruta), Some(&pista));
        assert_eq!(read_cache(Some(&ruta)), Some(pista));
    }

    // -- trayectoria -----------------------------------------------------------

    #[test]
    fn rellena_huecos_con_el_ultimo_valor_y_el_primero_si_empieza_vacio() {
        let huecos = [(0.0, None), (0.5, Some(0.3)), (1.0, None), (1.5, Some(0.6)), (2.0, None)];
        let lleno = fill_gaps(&huecos).unwrap();
        assert_eq!(lleno.iter().map(|p| p.1).collect::<Vec<_>>(), [0.3, 0.3, 0.3, 0.6, 0.6]);
        assert!(fill_gaps(&[(0.0, None), (0.5, None)]).is_none());
        assert!(fill_gaps(&[]).is_none());
    }

    #[test]
    fn el_suavizado_es_una_media_movil_acotada() {
        let puntos: Vec<(f64, f64)> = (0..10).map(|i| (i as f64 * 0.5, if i < 5 { 0.2 } else { 0.8 })).collect();
        let suave = smooth(&puntos, 2.0, 0.5);
        assert_eq!(suave.len(), 10);
        assert!((suave[0].1 - 0.2).abs() < 1e-9 && (suave[9].1 - 0.8).abs() < 1e-9);
        assert!(suave[4].1 > 0.2 && suave[5].1 < 0.8, "el salto se reparte");
        assert!(suave.windows(2).all(|w| w[0].1 <= w[1].1 + 1e-12), "sigue siendo monotono");
        assert_eq!(smooth(&puntos, 0.4, 0.5), puntos, "una ventana de una muestra no cambia nada");
    }

    #[test]
    fn los_puntos_clave_solo_recogen_cambios_de_verdad() {
        let plano: Vec<(f64, f64)> = (0..20).map(|i| (i as f64 * 0.5, 0.5 + (i % 2) as f64 * 0.01)).collect();
        let claves = to_keyframes(&plano, 0.04, 60);
        assert_eq!(claves, [(0.0, 0.5), (9.5, 0.51)], "el vaiven pequeno se ignora y se conserva el final");

        let salto = [(0.0, 0.2), (0.5, 0.2), (1.0, 0.7), (1.5, 0.7)];
        assert_eq!(to_keyframes(&salto, 0.04, 60), [(0.0, 0.2), (1.0, 0.7), (1.5, 0.7)]);
        assert!(to_keyframes(&[], 0.04, 60).is_empty());
    }

    #[test]
    fn el_tope_de_puntos_clave_conserva_el_ultimo() {
        let zigzag: Vec<(f64, f64)> = (0..200).map(|i| (i as f64, if i % 2 == 0 { 0.1 } else { 0.9 })).collect();
        let claves = to_keyframes(&zigzag, 0.04, 10);
        assert_eq!(claves.len(), 10);
        assert_eq!(*claves.last().unwrap(), (199.0, 0.9));
        assert_eq!(claves[0], (0.0, 0.1));
    }

    // -- eleccion de cara --------------------------------------------------------

    #[test]
    fn con_una_sola_cara_o_sin_historial_manda_la_mayor() {
        assert_eq!(pick(&[], Some(0.5)), None);
        let grande = Face::new(0.7, 5000.0, (0, 0, 0, 0));
        let pequena = Face::new(0.2, 1000.0, (0, 0, 0, 0));
        assert_eq!(pick(&[pequena.clone(), grande.clone()], None), Some(0.7));
        assert_eq!(pick(&[pequena.clone()], Some(0.9)), Some(0.2));
    }

    #[test]
    fn con_varias_caras_se_prefiere_la_mas_cercana_salvo_un_primer_plano() {
        let a = Face::new(0.3, 3000.0, (0, 0, 0, 0));
        let b = Face::new(0.7, 4000.0, (0, 0, 0, 0));
        assert_eq!(pick(&[a.clone(), b.clone()], Some(0.35)), Some(0.3), "no se salta por una cara apenas mayor");
        let primer_plano = Face::new(0.7, 9000.0, (0, 0, 0, 0));
        assert_eq!(pick(&[a, primer_plano], Some(0.35)), Some(0.7), "un primer plano si justifica el salto");
    }

    // -- fotos y carteles --------------------------------------------------------

    fn muestras(n: usize, f: impl Fn(usize) -> Vec<Face>) -> Vec<(f64, Vec<Face>)> {
        (0..n).map(|i| (i as f64 * 0.5, f(i))).collect()
    }

    #[test]
    fn una_cara_inmovil_junto_a_una_viva_se_descarta() {
        let datos = muestras(20, |_| vec![cara(0.25, 0.0, 0.1), cara(0.75, 0.0, 6.0)]);
        let limpias = drop_static_faces(datos, &cfg());
        assert!(limpias.iter().all(|(_, caras)| caras.len() == 1 && caras[0].cx == 0.75));
    }

    #[test]
    fn si_no_se_mueve_nada_no_se_descarta_nada() {
        let datos = muestras(20, |_| vec![cara(0.25, 0.0, 0.1), cara(0.75, 0.0, 0.2)]);
        assert_eq!(drop_static_faces(datos.clone(), &cfg()), datos);
    }

    #[test]
    fn una_aparicion_corta_no_basta_para_descartar() {
        let datos = muestras(3, |_| vec![cara(0.25, 0.0, 0.0), cara(0.75, 0.0, 9.0)]);
        assert_eq!(drop_static_faces(datos.clone(), &cfg()), datos, "hace falta presencia sostenida");
        let sin_filtro = ReframeConfig { ignore_static_faces: false, ..cfg() };
        let largo = muestras(20, |_| vec![cara(0.25, 0.0, 0.0), cara(0.75, 0.0, 9.0)]);
        assert_eq!(drop_static_faces(largo.clone(), &sin_filtro), largo);
    }

    // -- a quien se sigue ----------------------------------------------------------

    /// Dos personas; habla la de la izquierda los 10 primeros segundos y la de la
    /// derecha despues.
    fn conversacion() -> Vec<(f64, Vec<Face>)> {
        muestras(40, |i| {
            let (izq, der) = if i < 20 { (8.0, 0.5) } else { (0.5, 8.0) };
            vec![cara(0.25, izq, 3.0), cara(0.75, der, 3.0)]
        })
    }

    #[test]
    fn el_encuadre_sigue_a_quien_mueve_la_boca() {
        let centros = choose_centres(&conversacion(), &cfg(), None, 0.0);
        let al = |i: usize| centros[i].1.unwrap();
        assert_eq!(al(5), 0.25);
        assert_eq!(al(15), 0.25);
        assert_eq!(al(38), 0.75, "tras el cambio de turno pasa a la otra persona");
        // El cambio no es instantaneo: la otra tiene que ganar y mantenerlo.
        let primer_cambio = centros.iter().position(|(_, c)| *c == Some(0.75)).unwrap();
        assert!(primer_cambio >= 22, "salto demasiado pronto: {primer_cambio}");
    }

    #[test]
    fn un_gesto_suelto_no_cambia_el_plano() {
        let datos = muestras(40, |i| vec![cara(0.25, 8.0, 3.0), cara(0.75, if i == 20 { 30.0 } else { 0.5 }, 3.0)]);
        let centros = choose_centres(&datos, &cfg(), None, 0.0);
        assert!(centros.iter().all(|(_, c)| *c == Some(0.25)));
    }

    #[test]
    fn en_silencio_el_encuadre_no_se_mueve() {
        // La audiencia callada: el nivel de energia esta por debajo del umbral.
        let silencio = EnergyTrack::new(vec![0.0; 100], 0.25, 25.0, 0.0);
        let centros = choose_centres(&conversacion(), &cfg(), Some(&silencio), 0.0);
        let primero = centros[0].1;
        assert!(centros.iter().all(|(_, c)| *c == primero), "no hay nadie hablando: sin cambios de plano");
    }

    #[test]
    fn con_una_sola_persona_o_sin_boca_medible_manda_el_criterio_de_siempre() {
        let una = muestras(10, |_| vec![cara(0.5, 5.0, 3.0)]);
        assert!(speaker_centres(&una, &cfg(), None, 0.0).is_none());
        let sin_boca = muestras(10, |_| vec![cara(0.25, 0.0, 3.0), cara(0.75, 0.0, 3.0)]);
        assert!(speaker_centres(&sin_boca, &cfg(), None, 0.0).is_none());
        // Y aun asi sale una trayectoria por el otro camino.
        assert!(choose_centres(&sin_boca, &cfg(), None, 0.0).iter().all(|(_, c)| c.is_some()));
        let apagado = ReframeConfig { speaker_tracking: false, ..cfg() };
        assert!(choose_centres(&conversacion(), &apagado, None, 0.0).iter().all(|(_, c)| c.is_some()));
    }

    #[test]
    fn las_muestras_sin_cara_no_rompen_el_seguimiento() {
        let mut datos = conversacion();
        datos[3].1.clear();
        datos[4].1.clear();
        let centros = choose_centres(&datos, &cfg(), None, 0.0);
        assert_eq!(centros[3].1, None);
        assert_eq!(centros[5].1, Some(0.25));
    }

    // -- trayectoria completa ----------------------------------------------------------

    #[test]
    fn de_las_muestras_a_la_trayectoria() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("t.json");
        let track = track_from_samples(conversacion(), &cfg(), None, 0.0, Some(&cache)).unwrap();
        assert_eq!(track.detection_ratio, 1.0);
        assert!(track.points.len() >= 2 && track.points.first().unwrap().1 < 0.4 && track.points.last().unwrap().1 > 0.6);
        assert!(track.points.windows(2).all(|w| w[0].0 < w[1].0));
        assert_eq!(read_cache(Some(&cache)).unwrap().points.len(), track.points.len());
    }

    #[test]
    fn sin_caras_o_con_muy_pocas_se_recorta_por_el_centro_y_queda_anotado() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("vacio.json");
        assert!(track_from_samples(muestras(10, |_| vec![]), &cfg(), None, 0.0, Some(&cache)).is_none());
        assert!(read_cache(Some(&cache)).unwrap().points.is_empty(), "tambien se recuerda que no habia caras");
        assert!(track_from_samples(vec![], &cfg(), None, 0.0, None).is_none());

        // Una cara en 1 de cada 10 muestras no es fiable.
        let rala = muestras(40, |i| if i == 5 { vec![cara(0.5, 0.0, 3.0)] } else { vec![] });
        assert!(track_from_samples(rala, &cfg(), None, 0.0, None).is_none());
    }

    // -- fotogramas -----------------------------------------------------------------------

    #[test]
    fn tamano_de_deteccion() {
        assert_eq!(detection_size(1920, 1080), (480, 270));
        assert_eq!(detection_size(320, 240), (320, 240));
        let (w, h) = detection_size(1080, 1920); // vertical: no cabe a 480 de ancho
        assert!(w <= 640 && h <= 640 && (w as f64 / h as f64 - 0.5625).abs() < 0.02, "{w}x{h}");
        assert_eq!(detection_size(4000, 100), (480, 12));
    }

    #[test]
    fn gris_como_opencv() {
        let f = Frame { width: 2, height: 1, bgr: vec![0, 0, 255, 255, 255, 255] }; // rojo puro, blanco
        assert_eq!(f.gray(), [76, 255]);
    }

    #[test]
    fn el_movimiento_de_una_zona() {
        let (w, h) = (20usize, 20usize);
        let antes = vec![100u8; w * h];
        let mut despues = antes.clone();
        for y in 5..15 {
            for x in 5..15 {
                despues[y * w + x] = 110;
            }
        }
        assert_eq!(region_motion(&antes, &despues, w, h, (5, 5, 10, 10)), 10.0);
        assert_eq!(region_motion(&antes, &despues, w, h, (0, 0, 4, 20)), 0.0);
        assert_eq!(region_motion(&antes, &despues, w, h, (0, 0, 2, 2)), 0.0, "una caja minima no mide nada");
        assert_eq!(region_motion(&antes, &despues, w, h, (-5, -5, 3, 3)), 0.0);
        let media = region_motion(&antes, &despues, w, h, (0, 0, 20, 20));
        assert!((media - 2.5).abs() < 1e-9, "{media}");
    }

    #[test]
    fn la_boca_es_el_tercio_inferior_de_la_cara() {
        assert_eq!(mouth_box((60, 100, 60, 60)), (70, 136, 40, 24));
        assert_eq!(mouth_box((0, 0, 3, 3)), (0, 1, 2, 2));
    }

    #[test]
    fn el_muestreo_mide_el_movimiento_desde_la_segunda_muestra() {
        struct Fija;
        impl Detector for Fija {
            fn detect(&mut self, _: &Frame) -> Vec<Face> {
                vec![Face::new(0.5, 1600.0, (4, 4, 12, 12))]
            }
        }
        let quieto = Frame { width: 20, height: 20, bgr: vec![100; 20 * 20 * 3] };
        let mut cambia = quieto.clone();
        cambia.bgr = vec![130; 20 * 20 * 3];
        let muestras = sample(vec![quieto.clone(), cambia, quieto].into_iter(), &mut Fija, 0.5);

        assert_eq!(muestras.iter().map(|(t, _)| *t).collect::<Vec<_>>(), [0.0, 0.5, 1.0]);
        assert_eq!(muestras[0].1[0].motion, 0.0, "sin muestra anterior no hay movimiento");
        assert_eq!(muestras[1].1[0].motion, 30.0);
        assert_eq!(muestras[2].1[0].motion, 30.0);
    }

    // -- decodificacion de YuNet -------------------------------------------------------------

    fn mapas_vacios() -> Maps {
        let zeros = |n: usize, k: usize| vec![0f32; n * k];
        Maps {
            cls: [zeros(6400, 1), zeros(1600, 1), zeros(400, 1)],
            obj: [zeros(6400, 1), zeros(1600, 1), zeros(400, 1)],
            bbox: [zeros(6400, 4), zeros(1600, 4), zeros(400, 4)],
        }
    }

    #[test]
    fn una_celda_con_nota_alta_se_decodifica_a_su_caja() {
        let mut m = mapas_vacios();
        // Celda (fila 10, columna 20) del paso 8: idx = 10 * 80 + 20.
        let idx = 10 * 80 + 20;
        m.cls[0][idx] = 0.9;
        m.obj[0][idx] = 0.9;
        m.bbox[0][idx * 4..idx * 4 + 4].copy_from_slice(&[0.5, 0.5, 2.0f32.ln(), 3.0f32.ln()]); // w = 16, h = 24
        let cajas = decode(&m, 0.6, 0.3);
        assert_eq!(cajas.len(), 1);
        let b = cajas[0];
        assert!((b.score - 0.9).abs() < 1e-6);
        // centro = ((20 + 0.5) * 8, (10 + 0.5) * 8) = (164, 84)
        assert!((b.x - (164.0 - 8.0)).abs() < 1e-3 && (b.y - (84.0 - 12.0)).abs() < 1e-3, "{b:?}");
        assert!((b.w - 16.0).abs() < 1e-3 && (b.h - 24.0).abs() < 1e-3);
    }

    #[test]
    fn la_nota_baja_no_pasa_y_las_notas_fuera_de_rango_se_acotan() {
        let mut m = mapas_vacios();
        m.cls[1][5] = 0.5;
        m.obj[1][5] = 0.5; // nota 0.5 < 0.6
        m.cls[2][7] = 50.0; // se acota a 1
        m.obj[2][7] = 0.81;
        let cajas = decode(&m, 0.6, 0.3);
        assert_eq!(cajas.len(), 1);
        assert!((cajas[0].score - 0.9).abs() < 1e-6, "{}", cajas[0].score);
    }

    #[test]
    fn la_supresion_deja_la_mejor_caja_de_cada_grupo() {
        let a = RawBox { x: 100.0, y: 100.0, w: 60.0, h: 60.0, score: 0.8 };
        let casi_igual = RawBox { x: 104.0, y: 102.0, w: 60.0, h: 60.0, score: 0.95 };
        let lejos = RawBox { x: 400.0, y: 300.0, w: 60.0, h: 60.0, score: 0.7 };
        let tocando = RawBox { x: 150.0, y: 100.0, w: 60.0, h: 60.0, score: 0.6 }; // solape pequeno: se queda
        let salida = nms(vec![a, lejos, casi_igual, tocando], 0.3);
        assert_eq!(salida, vec![casi_igual, lejos, tocando]);
        assert!(nms(vec![], 0.3).is_empty());
    }

    // -- cache -----------------------------------------------------------------------------------

    #[test]
    fn la_cache_ilegible_o_ausente_se_ignora_y_la_clave_depende_del_tramo() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_cache(None).is_none());
        assert!(read_cache(Some(&dir.path().join("no.json"))).is_none());
        std::fs::write(dir.path().join("roto.json"), "{").unwrap();
        assert!(read_cache(Some(&dir.path().join("roto.json"))).is_none());

        let video = dir.path().join("v.mp4");
        std::fs::write(&video, b"x").unwrap();
        let a = cache_path(&video, 10.0, 30.0, &cfg(), Some(dir.path())).unwrap();
        assert_eq!(a, cache_path(&video, 10.0, 30.0, &cfg(), Some(dir.path())).unwrap());
        assert_ne!(a, cache_path(&video, 10.0, 31.0, &cfg(), Some(dir.path())).unwrap());
        assert_ne!(a, cache_path(&video, 10.0, 30.0, &ReframeConfig { deadzone: 0.1, ..cfg() }, Some(dir.path())).unwrap());
        assert!(cache_path(&video, 10.0, 30.0, &cfg(), None).is_none());
        assert!(cache_path(&dir.path().join("no_existe.mp4"), 0.0, 1.0, &cfg(), Some(dir.path())).is_none());
    }

    #[test]
    fn el_reencuadre_se_puede_apagar() {
        let off = ReframeConfig { enabled: false, ..cfg() };
        assert!(track_faces(Path::new("no_existe.mp4"), 0.0, 10.0, &off, None, None).is_none());
    }
}
