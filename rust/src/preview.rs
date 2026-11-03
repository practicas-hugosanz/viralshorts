//! Vista previa real de Crear: un fotograma montado por el mismo render que los
//! shorts, con la composicion, los subtitulos y la marca que hay elegidos.
//!
//! No hace falta descargar nada: sale de una miniatura de YouTube o de un
//! fotograma del archivo, convertidos en un clip de segundo y medio. Se monta
//! fuera de work/ para no molestar a un trabajo en marcha.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::Value;

use crate::config::Config;
use crate::jumpcuts::JumpCutPlan;
use crate::pipeline::{self, Args};
use crate::render::{self, RenderRequest};
use crate::words::{Track, Word};
use crate::{ffmpeg, gui, library, metadata, subtitles};

/// Lo que dura el clip de muestra, y de donde se saca el fotograma.
const CLIP_SECONDS: f64 = 1.6;
const FRAME_AT: f64 = 1.0;

const SAMPLE_WORDS: [&str; 6] = ["esto", "no", "te", "lo", "esperabas", "nunca"];
const SAMPLE_HOOK: &str = "¿POR QUÉ NADIE LO DICE?";
const SAMPLE_TITLE: &str = "El título de tu clip";

/// Solo una a la vez: comparten carpeta.
static BUSY: Mutex<()> = Mutex::new(());

fn folder(root: &Path) -> PathBuf {
    root.join("work").join("preview")
}

/// La ultima vista previa.
pub fn image_path(root: &Path) -> PathBuf {
    folder(root).join("vista.jpg")
}

/// Los ajustes elegidos en la pagina, sin lo que solo cuesta tiempo: el zoom, el
/// volumen, el codificador por hardware.
pub fn preview_config(root: &Path, form: &Value) -> Result<Config, String> {
    let file = root.join("config.yaml");
    let mut cfg = Config::load(file.is_file().then_some(file.as_path())).map_err(|e| e.to_string())?;
    for (setting, value) in gui::style_pairs(form) {
        cfg.set(&format!("{setting}={}", match &value {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        }))
        .map_err(|e| e.to_string())?;
    }
    if form["title_card"].as_bool() == Some(false) {
        cfg.title_card.enabled = false;
    }
    pipeline::apply_overrides(&mut cfg, &Args::default()).map_err(|e| e.to_string())?;
    cfg.zoom.enabled = false;
    cfg.audio.loudnorm = false;
    cfg.audio.music.clear();
    cfg.output.hardware_encoder = "off".into();
    cfg.output.preset = "ultrafast".into();
    Ok(cfg)
}

/// Palabras de muestra repartidas por el clip.
fn sample_words() -> Vec<Word> {
    let each = CLIP_SECONDS / SAMPLE_WORDS.len() as f64;
    SAMPLE_WORDS.iter().enumerate().map(|(i, word)| Word::new(*word, i as f64 * each, (i + 1) as f64 * each - 0.02)).collect()
}

/// El identificador de un enlace de YouTube.
pub fn youtube_id(text: &str) -> Option<String> {
    let re = regex::Regex::new(r"(?:youtube\.com/(?:watch\?(?:.*&)?v=|shorts/|live/|embed/)|youtu\.be/)([A-Za-z0-9_-]{11})").ok()?;
    Some(re.captures(text)?.get(1)?.as_str().to_string())
}

/// La imagen de la que partir, si se puede sacar de esa fuente.
fn still_for(source: &str) -> Option<Vec<u8>> {
    let source = source.split('|').next().unwrap_or("").trim();
    if source.is_empty() {
        return None;
    }
    if let Some(id) = youtube_id(source) {
        let url = format!("https://img.youtube.com/vi/{id}/hqdefault.jpg");
        let response = ureq::get(&url).timeout(std::time::Duration::from_secs(6)).call().ok()?;
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut response.into_reader().take_limit(), &mut bytes).ok()?;
        return (bytes.len() > 100).then_some(bytes);
    }
    if source.contains("://") {
        return None;
    }
    library::frame_jpeg(Path::new(source))
}

trait TakeLimit: std::io::Read + Sized {
    /// Una miniatura no pasa de unos cientos de KB.
    fn take_limit(self) -> std::io::Take<Self> {
        self.take(8 * 1024 * 1024)
    }
}
impl<R: std::io::Read> TakeLimit for R {}

/// Clip de muestra de una fuente: la misma fuente no se vuelve a preparar.
fn clip_for(dir: &Path, source: &str, placeholder: &str) -> Result<PathBuf, String> {
    let mut hasher = DefaultHasher::new();
    source.hash(&mut hasher);
    let modified = std::fs::metadata(source).and_then(|m| m.modified()).ok();
    modified.hash(&mut hasher);
    placeholder.hash(&mut hasher);
    let clip = dir.join(format!("fuente_{:016x}.mp4", hasher.finish()));
    if clip.is_file() {
        return Ok(clip);
    }

    let seconds = format!("{CLIP_SECONDS:.2}");
    let mut args: Vec<String> = Vec::new();
    match still_for(source) {
        Some(bytes) => {
            let still = clip.with_extension("jpg");
            std::fs::write(&still, bytes).map_err(|e| e.to_string())?;
            args.extend(["-loop".into(), "1".into(), "-i".into(), still.to_string_lossy().into_owned()]);
        }
        None => args.extend(["-f".into(), "lavfi".into(), "-i".into(), format!("color=c={placeholder}:s=1280x720:r=30")]),
    }
    args.extend(["-f".into(), "lavfi".into(), "-i".into(), "anullsrc=r=48000:cl=stereo".into()]);
    args.extend(
        ["-t", &seconds, "-r", "30", "-vf", "scale=trunc(iw/2)*2:trunc(ih/2)*2", "-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p", "-c:a", "aac", "-shortest"]
            .map(String::from),
    );
    args.push(clip.to_string_lossy().into_owned());
    ffmpeg::run(&args, None, None, "vista previa").map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(clip.with_extension("jpg"));
    Ok(clip)
}

/// Monta la vista previa y la deja en `image_path`.
pub fn render_preview(root: &Path, form: &Value) -> Result<(), String> {
    let _only_one = BUSY.lock().unwrap_or_else(|e| e.into_inner());
    let dir = std::path::absolute(folder(root)).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let cfg = preview_config(root, form)?;

    let text = |key: &str| form[key].as_str().unwrap_or("").to_string();
    let podcast = clip_for(&dir, &text("podcast"), "0x3a4358")?;
    let background = if pipeline::layout_needs_background(&cfg) { clip_for(&dir, &text("background"), "0x1f7a63")? } else { podcast.clone() };

    let words = sample_words();
    let title = metadata::card_text("Tu pódcast", SAMPLE_TITLE, &cfg);
    let mut content = subtitles::build_ass(
        &words,
        cfg.layout.width,
        cfg.layout.height,
        &cfg.subtitles,
        Some(&cfg.hook),
        Some(SAMPLE_HOOK),
        CLIP_SECONDS,
        Some(&cfg.title_card),
        Some(&title),
    )
    .map_err(|e| e.to_string())?;
    subtitles::append_handle(&mut content, &cfg.brand, cfg.layout.width, cfg.layout.height, CLIP_SECONDS, &cfg.subtitles, pipeline::logo_size(&cfg));
    subtitles::write_ass(&content, &dir.join("vista.ass")).map_err(|e| e.to_string())?;

    // Sin caras que buscar: en "dos personas" se reparte a ojo.
    let track = (cfg.layout.mode == "duo").then(|| {
        let mut track = Track::new(vec![(0.0, 0.5), (1.0, 0.5)], 1.0);
        track.duo = Some((0.32, 0.68));
        track
    });
    let output = dir.join("vista.mp4");
    let request = RenderRequest {
        podcast,
        background,
        start: 0.0,
        end: CLIP_SECONDS,
        background_offset: 0.0,
        plan: JumpCutPlan::full(CLIP_SECONDS),
        track,
        ass_name: Some("vista.ass".into()),
        output: output.clone(),
        work_dir: dir.clone(),
        beeps: Vec::new(),
        whooshes: Vec::new(),
        emojis: Vec::new(),
    };
    render::render(&request, &cfg).map_err(|e| e.to_string())?;

    let image = dir.join("vista-nueva.jpg");
    let args: Vec<String> = ["-ss", &format!("{FRAME_AT:.2}"), "-i", &output.to_string_lossy(), "-frames:v", "1", "-vf", "scale=540:-2", "-q:v", "4", &image.to_string_lossy()]
        .map(String::from)
        .to_vec();
    ffmpeg::run(&args, None, None, "vista previa").map_err(|e| e.to_string())?;
    std::fs::rename(&image, image_path(root)).map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(&output);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn la_vista_previa_lleva_lo_elegido_sin_lo_que_solo_cuesta_tiempo() {
        let dir = tempfile::tempdir().unwrap();
        let form = json!({"title_card": false, "style": {"layout_mode": "fit", "sub_style": "neon", "subs": "false", "hook": "true", "font_size": 80, "encoder": "auto"}});
        let cfg = preview_config(dir.path(), &form).unwrap();
        assert_eq!((cfg.layout.mode.as_str(), cfg.subtitles.style.as_str(), cfg.subtitles.font_size), ("fit", "neon", 80));
        assert!(!cfg.subtitles.enabled && cfg.hook.enabled && !cfg.title_card.enabled);
        assert!(!cfg.zoom.enabled && !cfg.audio.loudnorm);
        assert_eq!((cfg.output.hardware_encoder.as_str(), cfg.output.preset.as_str()), ("off", "ultrafast"));
    }

    #[test]
    fn las_palabras_de_muestra_llenan_el_clip_sin_pisarse() {
        let words = sample_words();
        assert!(words.windows(2).all(|w| w[0].end <= w[1].start));
        assert!(words.last().unwrap().end <= CLIP_SECONDS);
        assert!(words.iter().any(|w| w.start <= FRAME_AT && w.end >= FRAME_AT - 0.1), "en el fotograma suena una");
    }

    #[test]
    fn solo_se_piden_miniaturas_de_enlaces_de_youtube() {
        assert_eq!(youtube_id("https://www.youtube.com/watch?v=dQw4w9WgXcQ&t=3").as_deref(), Some("dQw4w9WgXcQ"));
        assert_eq!(youtube_id("https://youtu.be/dQw4w9WgXcQ").as_deref(), Some("dQw4w9WgXcQ"));
        assert_eq!(youtube_id("https://www.youtube.com/shorts/dQw4w9WgXcQ").as_deref(), Some("dQw4w9WgXcQ"));
        assert_eq!(youtube_id("https://evil.example/x/../watch?v=corto"), None);
        assert_eq!(youtube_id("C:/videos/mi.mp4"), None);
        assert_eq!(still_for(""), None);
        assert_eq!(still_for("https://vimeo.com/123"), None, "de otros sitios no se descarga nada");
    }
}
