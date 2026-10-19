//! Descarga de videos con yt-dlp, con cache y extraccion del heatmap.
//!
//! Ademas del fichero de video se guarda el `info.json` de yt-dlp porque
//! contiene el campo `heatmap` ("most replayed" de YouTube): la senal mas barata
//! y mas fiable que existe para saber que parte de un video engancha, porque la
//! ha generado la propia audiencia rebobinando.
//!
//! yt-dlp se usa como ejecutable (lo instala el instalador junto a ffmpeg).

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

use crate::config::DownloadConfig;
use crate::ffmpeg::{self, command};
use crate::progress;

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct DownloadError(pub String);

type Result<T> = std::result::Result<T, DownloadError>;

const VIDEO_EXTS: [&str; 6] = ["mp4", "mkv", "webm", "mov", "m4v", "avi"];

/// De todo lo que devuelve yt-dlp (cientos de campos, incluida la lista
/// completa de formatos) solo se usa esto.
const KEEP: [&str; 15] = [
    "id", "title", "duration", "webpage_url", "original_url", "uploader", "channel",
    "upload_date", "heatmap", "chapters", "language", "view_count", "like_count", "is_live", "was_live",
];

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HeatPoint {
    pub start: f64,
    pub end: f64,
    /// 0..1, relativo al resto del video.
    pub value: f64,
}

#[derive(Debug, Clone)]
pub struct SourceVideo {
    pub path: PathBuf,
    pub title: String,
    pub duration: f64,
    pub heatmap: Vec<HeatPoint>,
    pub info: Value,
}

impl SourceVideo {
    pub fn has_heatmap(&self) -> bool {
        self.heatmap.len() > 1
    }

    pub fn webpage_url(&self) -> &str {
        self.info["webpage_url"].as_str().unwrap_or("")
    }
}

/// Lo que se le pide a yt-dlp. Es un trait para poder probar `fetch` sin red.
pub trait YtDlp {
    /// Descarga el video en `<dest_base>.<ext>` y devuelve los metadatos.
    fn download(&self, url: &str, dest_base: &Path, cfg: &DownloadConfig) -> Result<Value>;
    /// Solo los metadatos, sin bajar el video.
    fn metadata(&self, url: &str, cfg: &DownloadConfig) -> Result<Value>;
}

/// Devuelve un `SourceVideo` para una URL o una ruta local. Si `url` apunta a
/// un fichero que ya existe, se usa tal cual; si ya hay una descarga previa con
/// el mismo `dest_base`, se reutiliza en vez de volver a bajar el video.
pub fn fetch(url: &str, dest_base: &Path, cfg: &DownloadConfig) -> Result<SourceVideo> {
    fetch_with(&YtDlpCli, url, dest_base, cfg)
}

pub fn fetch_with(ytdlp: &dyn YtDlp, url: &str, dest_base: &Path, cfg: &DownloadConfig) -> Result<SourceVideo> {
    let local = Path::new(url);
    if local.is_file() {
        log::info!("Usando fichero local: {}", local.display());
        return from_local_file(local);
    }

    if let Some(parent) = dest_base.parent() {
        std::fs::create_dir_all(parent).map_err(|e| DownloadError(format!("No se pudo crear {}: {e}", parent.display())))?;
    }
    let info_path = info_path(dest_base);

    if let Some(cached) = find_existing(dest_base) {
        log::info!("Reutilizando descarga previa: {}", cached.file_name().unwrap_or_default().to_string_lossy());
        let info = cached_info(ytdlp, url, &info_path, cfg);
        return build(cached, info);
    }

    log::info!("Descargando {url}");
    let info = ytdlp.download(url, dest_base, cfg)?;
    let Some(downloaded) = find_existing(dest_base) else {
        return Err(DownloadError(format!(
            "yt-dlp dijo que descargo '{url}' pero no hay ningun fichero en {} que empiece por '{}'.",
            dest_base.parent().unwrap_or(Path::new(".")).display(),
            dest_base.file_name().unwrap_or_default().to_string_lossy()
        )));
    };
    save_info(&info, &info_path);
    build(downloaded, info)
}

fn info_path(dest_base: &Path) -> PathBuf {
    let mut name = dest_base.file_name().unwrap_or_default().to_os_string();
    name.push(".info.json");
    dest_base.with_file_name(name)
}

/// Metadatos de una descarga previa, recuperandolos si hiciera falta. Se piden
/// SIN volver a descargar el video: bajar otra vez varios GB por un heatmap no
/// compensa.
fn cached_info(ytdlp: &dyn YtDlp, url: &str, info_path: &Path, cfg: &DownloadConfig) -> Value {
    if info_path.is_file() {
        match std::fs::read_to_string(info_path).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()) {
            Some(info) => return info,
            None => log::warn!("{} ilegible; se vuelven a pedir los metadatos.", info_path.file_name().unwrap_or_default().to_string_lossy()),
        }
    }

    log::info!("Recuperando metadatos (sin volver a descargar el video)...");
    match ytdlp.metadata(url, cfg) {
        Ok(info) => {
            save_info(&info, info_path);
            info
        }
        Err(error) => {
            log::warn!("No se pudieron recuperar los metadatos ({error}). Se sigue sin heatmap.");
            json!({})
        }
    }
}

/// Guarda los metadatos que se usan. Nunca aborta el pipeline.
fn save_info(info: &Value, info_path: &Path) {
    if let Err(error) = std::fs::write(info_path, info.to_string()) {
        log::warn!("No se pudo guardar {} ({error}); no es critico.", info_path.display());
    }
}

/// Busca un fichero ya descargado, priorizando mp4 y despues el mas grande.
pub fn find_existing(dest_base: &Path) -> Option<PathBuf> {
    let parent = dest_base.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let prefix = format!("{}.", dest_base.file_name()?.to_string_lossy());
    let mut candidates: Vec<(PathBuf, u64)> = std::fs::read_dir(parent)
        .ok()?
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_name().to_string_lossy().starts_with(&prefix))
        .filter(|entry| {
            let ext = entry.path().extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
            VIDEO_EXTS.contains(&ext.as_str())
        })
        .filter_map(|entry| {
            let size = entry.metadata().ok()?.len();
            (size > 0).then(|| (entry.path(), size))
        })
        .collect();
    candidates.sort_by_key(|(path, size)| {
        let is_mp4 = path.extension().is_some_and(|e| e.eq_ignore_ascii_case("mp4"));
        (!is_mp4, std::cmp::Reverse(*size))
    });
    candidates.into_iter().next().map(|(path, _)| path)
}

fn build(path: PathBuf, info: Value) -> Result<SourceVideo> {
    let duration = match info["duration"].as_f64().filter(|d| *d > 0.0) {
        Some(duration) => duration,
        None => ffmpeg::probe(&path).map_err(|e| DownloadError(e.to_string()))?.duration,
    };
    let heatmap = parse_heatmap(&info);
    if heatmap.is_empty() {
        log::info!("Este video no expone heatmap; se puntuara sin esa senal.");
    } else {
        log::info!("Heatmap de YouTube disponible ({} puntos): se usara para puntuar.", heatmap.len());
    }
    let title = info["title"].as_str().filter(|t| !t.is_empty()).map(str::to_string).unwrap_or_else(|| {
        path.file_stem().unwrap_or_default().to_string_lossy().into_owned()
    });
    Ok(SourceVideo { path, title, duration, heatmap, info })
}

fn from_local_file(path: &Path) -> Result<SourceVideo> {
    // Un fichero local puede tener al lado el info.json de una descarga previa.
    let info_path = path.with_extension("info.json");
    let mut info = json!({});
    if info_path.is_file() {
        match std::fs::read_to_string(&info_path).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()) {
            Some(read) => info = read,
            None => log::warn!("No se pudo leer {}; se ignora.", info_path.display()),
        }
    }
    let duration = ffmpeg::probe(path).map_err(|e| DownloadError(e.to_string()))?.duration;
    let title = info["title"].as_str().filter(|t| !t.is_empty()).map(str::to_string).unwrap_or_else(|| {
        path.file_stem().unwrap_or_default().to_string_lossy().into_owned()
    });
    let heatmap = parse_heatmap(&info);
    Ok(SourceVideo { path: path.to_path_buf(), title, duration, heatmap, info })
}

pub fn parse_heatmap(info: &Value) -> Vec<HeatPoint> {
    let Some(raw) = info["heatmap"].as_array() else { return Vec::new() };
    let mut points: Vec<HeatPoint> = raw
        .iter()
        .filter_map(|item| {
            Some(HeatPoint {
                start: item["start_time"].as_f64()?,
                end: item["end_time"].as_f64()?,
                value: item["value"].as_f64()?,
            })
        })
        .collect();
    points.sort_by(|a, b| a.start.total_cmp(&b.start));
    points
}

/// Se queda con una lista de campos y, si yt-dlp devolvio una lista de
/// reproduccion (mixes, canales, directos), con su primer video.
pub fn slim(info: &Value) -> Result<Value> {
    let mut info = info;
    if info["_type"] == "playlist" {
        info = info["entries"]
            .as_array()
            .and_then(|entries| entries.iter().find(|e| !e.is_null()))
            .ok_or_else(|| DownloadError("La URL no devolvio ningun video.".into()))?;
    }
    let mut out = Map::new();
    for key in KEEP {
        if let Some(value) = info.get(key).filter(|v| !v.is_null()) {
            out.insert(key.to_string(), value.clone());
        }
    }
    Ok(Value::Object(out))
}

/// El selector de formato de yt-dlp, con la altura maxima puesta. Un selector
/// escrito a mano en config.yaml se usa tal cual.
pub fn format_string(cfg: &DownloadConfig) -> String {
    cfg.format.replace("{max_height}", &cfg.max_height.to_string())
}

// ----------------------------------------------------------------------------
// yt-dlp como ejecutable
// ----------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
pub enum Mode<'a> {
    Download { dest_base: &'a Path },
    MetadataOnly,
}

/// Argumentos de yt-dlp. Se puede probar sin ejecutarlo.
pub fn ytdlp_args(url: &str, cfg: &DownloadConfig, mode: Mode) -> Vec<String> {
    let mut args: Vec<String> = ["--ignore-config", "--no-playlist", "--no-warnings", "--no-colors"]
        .map(String::from)
        .to_vec();
    args.extend(["--retries".into(), cfg.retries.to_string()]);
    if let Some(browser) = cfg.cookies_from_browser.as_deref().filter(|b| !b.is_empty()) {
        args.extend(["--cookies-from-browser".into(), browser.into()]);
    }
    if let Some(file) = cfg.cookiefile.as_deref().filter(|f| !f.is_empty()) {
        args.extend(["--cookies".into(), file.into()]);
    }

    match mode {
        Mode::MetadataOnly => args.extend(["--skip-download".into(), "--dump-single-json".into()]),
        Mode::Download { dest_base } => {
            args.extend([
                "-f".into(), format_string(cfg),
                "--merge-output-format".into(), "mp4".into(),
                // YouTube sirve los videos largos en cientos de fragmentos y
                // yt-dlp los pide de uno en uno: bajarlos en paralelo es la
                // mejora mas barata del pipeline.
                "--concurrent-fragments".into(), cfg.concurrent_fragments.max(1).to_string(),
                "--http-chunk-size".into(), cfg.http_chunk_size.max(1 << 20).to_string(),
                "-o".into(), format!("{}.%(ext)s", dest_base.display()),
                "--write-info-json".into(),
                // Nuestra propia barra: la de yt-dlp usa retorno de carro y en
                // un log sin terminal se convierte en una linea de cientos de KB.
                "--newline".into(), "--progress".into(),
                "--progress-template".into(),
                "download:@@DL %(progress.status)s %(progress.downloaded_bytes)s %(progress.total_bytes)s %(progress.total_bytes_estimate)s".into(),
            ]);
        }
    }
    // `--` evita que una "URL" que empiece por guion se tome por una opcion.
    args.extend(["--".into(), url.into()]);
    args
}

/// Avance que imprime yt-dlp con nuestra plantilla.
#[derive(Debug, PartialEq)]
pub enum DownloadEvent {
    Downloading { done: f64, total: f64 },
    Finished,
}

pub fn parse_progress_line(line: &str) -> Option<DownloadEvent> {
    let mut parts = line.trim().strip_prefix("@@DL ")?.split_whitespace();
    let status = parts.next()?;
    let mut number = || parts.next().and_then(|v| v.parse::<f64>().ok());
    let (done, total, estimate) = (number(), number(), number());
    match status {
        "finished" => Some(DownloadEvent::Finished),
        "downloading" => {
            let (done, total) = (done?, total.or(estimate)?);
            (done > 0.0 && total > 0.0).then_some(DownloadEvent::Downloading { done, total })
        }
        _ => None,
    }
}

pub struct YtDlpCli;

impl YtDlpCli {
    fn executable() -> Result<PathBuf> {
        ffmpeg::which("yt-dlp").ok_or_else(|| {
            DownloadError(
                "Falta yt-dlp. Doble clic en viralshorts.bat lo instala, o descarga yt-dlp.exe de \
                 https://github.com/yt-dlp/yt-dlp/releases y dejalo en la carpeta bin."
                    .into(),
            )
        })
    }

    fn run(args: &[String], on_progress: bool) -> Result<(String, Vec<String>)> {
        let exe = Self::executable()?;
        let mut child = command(exe)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| DownloadError(format!("No se pudo lanzar yt-dlp: {e}")))?;

        // stderr en su hilo: con los dos tubos sin vaciar, yt-dlp se bloquea.
        let stderr = child.stderr.take().expect("stderr encauzado");
        let errors = thread::spawn(move || {
            let mut lines = Vec::new();
            for line in BufReader::new(stderr).lines().map_while(std::result::Result::ok) {
                if !line.trim().is_empty() {
                    lines.push(line);
                }
            }
            lines
        });

        let stdout = child.stdout.take().expect("stdout encauzado");
        let last_report = Mutex::new(Instant::now() - Duration::from_secs(1));
        let mut captured = String::new();
        for line in BufReader::new(stdout).lines().map_while(std::result::Result::ok) {
            if on_progress {
                match parse_progress_line(&line) {
                    Some(DownloadEvent::Finished) => progress::progress("descarga", 1.0, "descargando", true),
                    Some(DownloadEvent::Downloading { done, total }) => {
                        // El hook salta muchas veces por segundo; sin freno se llenaria el log.
                        let mut last = last_report.lock().unwrap();
                        if last.elapsed() >= Duration::from_millis(500) {
                            *last = Instant::now();
                            progress::progress(
                                "descarga",
                                done / total,
                                &format!("descargando {:.0}/{:.0} MB", done / 1e6, total / 1e6),
                                false,
                            );
                        }
                    }
                    None => {}
                }
            } else {
                captured.push_str(&line);
                captured.push('\n');
            }
        }

        let status = child.wait().map_err(|e| DownloadError(format!("yt-dlp no termino: {e}")))?;
        let stderr_lines = errors.join().unwrap_or_default();
        if !status.success() {
            let tail: Vec<&String> = stderr_lines.iter().rev().take(12).collect::<Vec<_>>().into_iter().rev().collect();
            let detail = tail.iter().map(|l| l.as_str()).collect::<Vec<_>>().join("\n");
            return Err(DownloadError(if detail.is_empty() {
                format!("yt-dlp fallo (codigo {}).", status.code().unwrap_or(-1))
            } else {
                detail
            }));
        }
        Ok((captured, stderr_lines))
    }
}

impl YtDlp for YtDlpCli {
    fn download(&self, url: &str, dest_base: &Path, cfg: &DownloadConfig) -> Result<Value> {
        Self::run(&ytdlp_args(url, cfg, Mode::Download { dest_base }), true)?;

        // `--write-info-json` deja el info completo junto al video: se
        // sustituye por el reducido.
        let path = info_path(dest_base);
        let text = std::fs::read_to_string(&path)
            .map_err(|e| DownloadError(format!("yt-dlp no dejo los metadatos en {}: {e}", path.display())))?;
        let full: Value = serde_json::from_str(&text).map_err(|e| DownloadError(format!("Metadatos ilegibles: {e}")))?;
        slim(&full)
    }

    fn metadata(&self, url: &str, cfg: &DownloadConfig) -> Result<Value> {
        let (stdout, _) = Self::run(&ytdlp_args(url, cfg, Mode::MetadataOnly), false)?;
        let full: Value = serde_json::from_str(stdout.trim()).map_err(|e| DownloadError(format!("Metadatos ilegibles: {e}")))?;
        slim(&full)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[test]
    fn heatmap_de_youtube() {
        let info = json!({"heatmap": [
            {"start_time": 10.0, "end_time": 20.0, "value": 0.5},
            {"start_time": 0.0, "end_time": 10.0, "value": 1.0},
            {"start_time": 20.0, "value": 0.2},
            "basura",
        ]});
        let mapa = parse_heatmap(&info);
        assert_eq!(mapa, vec![HeatPoint { start: 0.0, end: 10.0, value: 1.0 }, HeatPoint { start: 10.0, end: 20.0, value: 0.5 }]);
        assert!(parse_heatmap(&json!({})).is_empty());
        assert!(parse_heatmap(&json!({"heatmap": "no"})).is_empty());
    }

    #[test]
    fn solo_se_guardan_los_campos_que_se_usan() {
        let full = json!({"id": "abc", "title": "T", "duration": 60, "formats": [1, 2, 3], "heatmap": [], "view_count": null, "uploader": "U"});
        let slim = slim(&full).unwrap();
        assert_eq!(slim, json!({"id": "abc", "title": "T", "duration": 60, "heatmap": [], "uploader": "U"}));
    }

    #[test]
    fn una_lista_de_reproduccion_se_queda_con_el_primer_video() {
        let lista = json!({"_type": "playlist", "entries": [null, {"id": "uno", "title": "Uno"}, {"id": "dos"}]});
        assert_eq!(slim(&lista).unwrap()["id"], "uno");
        assert!(slim(&json!({"_type": "playlist", "entries": []})).is_err());
    }

    #[test]
    fn el_formato_lleva_la_altura_maxima() {
        let cfg = DownloadConfig { max_height: 720, ..Default::default() };
        assert!(format_string(&cfg).contains("height<=720"));
        assert!(!format_string(&cfg).contains('{'));
        let propio = DownloadConfig { format: "best[ext=mp4]".into(), ..Default::default() };
        assert_eq!(format_string(&propio), "best[ext=mp4]");
    }

    #[test]
    fn argumentos_de_yt_dlp() {
        let cfg = DownloadConfig { cookies_from_browser: Some("firefox".into()), concurrent_fragments: 0, http_chunk_size: 10, ..Default::default() };
        let args = ytdlp_args("https://youtu.be/x", &cfg, Mode::Download { dest_base: Path::new("work/podcast") });
        let tras = |flag: &str| args[args.iter().position(|a| a == flag).unwrap() + 1].clone();
        assert_eq!(tras("--concurrent-fragments"), "1");
        assert_eq!(tras("--http-chunk-size"), (1 << 20).to_string());
        assert_eq!(tras("--cookies-from-browser"), "firefox");
        assert_eq!(tras("-o"), "work/podcast.%(ext)s");
        assert!(args.contains(&"--write-info-json".to_string()) && args.contains(&"--no-playlist".to_string()));
        assert_eq!(args[args.len() - 2..], ["--", "https://youtu.be/x"]);

        let meta = ytdlp_args("u", &DownloadConfig::default(), Mode::MetadataOnly);
        assert!(meta.contains(&"--skip-download".to_string()) && meta.contains(&"--dump-single-json".to_string()));
        assert!(!meta.contains(&"-o".to_string()));
    }

    #[test]
    fn una_url_que_parece_opcion_no_se_cuela() {
        let args = ytdlp_args("--exec=calc", &DownloadConfig::default(), Mode::MetadataOnly);
        let separador = args.iter().position(|a| a == "--").unwrap();
        assert_eq!(args[separador + 1], "--exec=calc");
        assert_eq!(separador, args.len() - 2);
    }

    #[test]
    fn lineas_de_progreso() {
        assert_eq!(parse_progress_line("@@DL downloading 5000000 10000000 NA"), Some(DownloadEvent::Downloading { done: 5e6, total: 1e7 }));
        assert_eq!(parse_progress_line("@@DL downloading 5000000 NA 20000000"), Some(DownloadEvent::Downloading { done: 5e6, total: 2e7 }));
        assert_eq!(parse_progress_line("@@DL downloading NA NA NA"), None);
        assert_eq!(parse_progress_line("@@DL downloading 0 100 NA"), None);
        assert_eq!(parse_progress_line("@@DL finished 10 10 NA"), Some(DownloadEvent::Finished));
        assert_eq!(parse_progress_line("[youtube] Extracting URL"), None);
    }

    // -- fetch con un yt-dlp de mentira ---------------------------------------

    #[derive(Default)]
    struct Falso {
        llamadas: RefCell<Vec<String>>,
        crea_video: bool,
    }

    impl YtDlp for Falso {
        fn download(&self, _url: &str, dest_base: &Path, _cfg: &DownloadConfig) -> Result<Value> {
            self.llamadas.borrow_mut().push("download".into());
            if self.crea_video {
                std::fs::write(dest_base.with_extension("mp4"), b"video").unwrap();
            }
            Ok(json!({"title": "Mi podcast", "duration": 3600.0, "heatmap": [{"start_time": 0.0, "end_time": 5.0, "value": 1.0}, {"start_time": 5.0, "end_time": 10.0, "value": 0.1}]}))
        }

        fn metadata(&self, _url: &str, _cfg: &DownloadConfig) -> Result<Value> {
            self.llamadas.borrow_mut().push("metadata".into());
            Ok(json!({"title": "Recuperado", "duration": 120.0}))
        }
    }

    #[test]
    fn descarga_guarda_los_metadatos_y_devuelve_el_video() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("podcast");
        let falso = Falso { crea_video: true, ..Default::default() };
        let video = fetch_with(&falso, "https://youtu.be/x", &base, &DownloadConfig::default()).unwrap();
        assert_eq!(video.title, "Mi podcast");
        assert_eq!(video.duration, 3600.0);
        assert!(video.has_heatmap());
        assert!(video.path.ends_with("podcast.mp4"));
        assert!(dir.path().join("podcast.info.json").is_file());
    }

    #[test]
    fn la_segunda_vez_reutiliza_la_descarga_sin_llamar_a_yt_dlp() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("podcast");
        let falso = Falso { crea_video: true, ..Default::default() };
        fetch_with(&falso, "u", &base, &DownloadConfig::default()).unwrap();
        fetch_with(&falso, "u", &base, &DownloadConfig::default()).unwrap();
        assert_eq!(*falso.llamadas.borrow(), ["download"]);
    }

    #[test]
    fn si_faltan_los_metadatos_se_piden_sin_volver_a_descargar() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("podcast");
        std::fs::write(dir.path().join("podcast.mp4"), b"video").unwrap();
        let falso = Falso::default();
        let video = fetch_with(&falso, "u", &base, &DownloadConfig::default()).unwrap();
        assert_eq!(*falso.llamadas.borrow(), ["metadata"]);
        assert_eq!(video.title, "Recuperado");
        assert!(dir.path().join("podcast.info.json").is_file(), "se guardan para la proxima");
    }

    #[test]
    fn metadatos_ilegibles_se_vuelven_a_pedir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("podcast.mp4"), b"video").unwrap();
        std::fs::write(dir.path().join("podcast.info.json"), "{roto").unwrap();
        let falso = Falso::default();
        fetch_with(&falso, "u", &dir.path().join("podcast"), &DownloadConfig::default()).unwrap();
        assert_eq!(*falso.llamadas.borrow(), ["metadata"]);
    }

    #[test]
    fn si_yt_dlp_no_deja_ningun_fichero_el_error_lo_dice() {
        let dir = tempfile::tempdir().unwrap();
        let falso = Falso::default(); // no crea el video
        let error = fetch_with(&falso, "u", &dir.path().join("podcast"), &DownloadConfig::default()).unwrap_err().to_string();
        assert!(error.contains("no hay ningun fichero") && error.contains("podcast"), "{error}");
    }

    #[test]
    fn se_prefiere_mp4_y_se_ignoran_vacios_y_otros_ficheros() {
        let dir = tempfile::tempdir().unwrap();
        let w = |name: &str, bytes: &[u8]| std::fs::write(dir.path().join(name), bytes).unwrap();
        w("podcast.webm", b"un webm bastante mas grande que el mp4");
        w("podcast.mp4", b"mp4");
        w("podcast.mkv", b"");
        w("podcast.info.json", b"{}");
        w("otro.mp4", b"no es este");
        let encontrado = find_existing(&dir.path().join("podcast")).unwrap();
        assert!(encontrado.ends_with("podcast.mp4"));
        std::fs::remove_file(dir.path().join("podcast.mp4")).unwrap();
        assert!(find_existing(&dir.path().join("podcast")).unwrap().ends_with("podcast.webm"));
        assert!(find_existing(&dir.path().join("nada")).is_none());
    }

    #[test]
    fn un_fichero_local_no_pasa_por_yt_dlp() {
        // Sin ffprobe no se puede medir: la prueba real vive en tests/.
        let falso = Falso::default();
        let dir = tempfile::tempdir().unwrap();
        let video = dir.path().join("v.mp4");
        std::fs::write(&video, b"x").unwrap();
        let _ = fetch_with(&falso, video.to_str().unwrap(), &dir.path().join("podcast"), &DownloadConfig::default());
        assert!(falso.llamadas.borrow().is_empty());
    }
}
