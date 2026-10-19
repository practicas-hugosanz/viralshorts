//! Envoltorio fino sobre ffmpeg/ffprobe.
//!
//! Todo el trabajo de video del pipeline pasa por aqui. Una sola llamada a
//! ffmpeg con un `filter_complex` evita recodificar el video varias veces.

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::progress;

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct FfmpegError(pub String);

type Result<T> = std::result::Result<T, FfmpegError>;

// ----------------------------------------------------------------------------
// Procesos y herramientas portables
// ----------------------------------------------------------------------------

/// `Command` que en Windows no abre una ventana de consola por cada proceso.
pub fn command<S: AsRef<OsStr>>(program: S) -> Command {
    #[allow(unused_mut)]
    let mut cmd = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    cmd
}

/// Anade al PATH las herramientas que descarga el instalador: `bin/ffmpeg/bin`
/// (ffmpeg y ffprobe) y `bin/` (yt-dlp, whisper.cpp). Solo afecta a este
/// proceso y a los que lance; no se toca la configuracion del ordenador.
pub fn use_portable_tools(root: &Path) {
    let mut extra: Vec<PathBuf> = Vec::new();
    for candidate in [root.join("bin").join("ffmpeg").join("bin"), root.join("bin")] {
        if candidate.is_dir() {
            extra.push(candidate);
        }
    }
    if extra.is_empty() {
        return;
    }
    let current = std::env::var_os("PATH").unwrap_or_default();
    let mut parts: Vec<PathBuf> = std::env::split_paths(&current).collect();
    for dir in extra.into_iter().rev() {
        if !parts.contains(&dir) {
            parts.insert(0, dir);
        }
    }
    if let Ok(joined) = std::env::join_paths(parts) {
        std::env::set_var("PATH", joined);
    }
}

/// Carpeta de datos del programa (herramientas, modelos, trabajo, configuracion).
///
///  1. `VIRALSHORTS_HOME`, si esta puesta.
///  2. Modo portable: la carpeta del ejecutable (o hasta tres por encima, para
///     una copia de desarrollo `rust/target/release`) si tiene `config.yaml` o
///     `bin/` al lado.
///  3. Instalado: `%LOCALAPPDATA%\viralshorts`, que es donde la aplicacion
///     guarda todo cuando vive en `Program Files` o en `Programs`.
pub fn project_root() -> PathBuf {
    if let Some(home) = std::env::var_os("VIRALSHORTS_HOME") {
        return PathBuf::from(home);
    }
    if let Some(dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)) {
        for candidate in dir.ancestors().take(4) {
            if candidate.join("config.yaml").is_file() || candidate.join("bin").is_dir() {
                return candidate.to_path_buf();
            }
        }
    }
    data_dir()
}

/// `%LOCALAPPDATA%\viralshorts` (o el equivalente en otros sistemas).
pub fn data_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("XDG_DATA_HOME").map(PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local").join("share")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("viralshorts")
}

/// Donde se guardan los shorts cuando el usuario no ha elegido otra cosa:
/// `Videos\viralshorts` en su carpeta personal.
pub fn default_output_dir() -> PathBuf {
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).map(PathBuf::from);
    match home {
        Some(home) => home.join("Videos").join("viralshorts"),
        None => PathBuf::from("output"),
    }
}

/// La configuracion que se escribe la primera vez en modo instalado: la del
/// proyecto, con la carpeta de salida en Videos.
pub fn first_config(template: &str, output_dir: &Path) -> String {
    let wanted = format!("  dir: \"{}\"", output_dir.display().to_string().replace('\\', "/"));
    // Con fin de linea de Windows o de Unix, da igual como llegue el fichero.
    let text = template.replace("\r\n", "\n");

    let mut lines: Vec<String> = Vec::new();
    let mut section_at: Option<usize> = None; // donde esta "output:"
    let mut inside = false;
    let mut replaced = false;
    for line in text.lines() {
        if line == "output:" {
            section_at = Some(lines.len());
            inside = true;
        } else if inside && !line.is_empty() && !line.starts_with(' ') && !line.starts_with('#') {
            inside = false; // otra seccion
        }
        if inside && !replaced && line.starts_with("  dir:") {
            lines.push(wanted.clone());
            replaced = true;
        } else {
            lines.push(line.to_string());
        }
    }
    match (replaced, section_at) {
        (true, _) => {}
        (false, Some(at)) => lines.insert(at + 1, wanted), // la seccion existe pero sin `dir`
        (false, None) => {
            lines.push("output:".into());
            lines.push(wanted);
        }
    }
    lines.join("\n") + "\n"
}

/// Deja lista la carpeta de datos del modo instalado: existe y tiene un
/// config.yaml (con la salida en Videos). En modo portable no toca nada.
pub fn ensure_home(root: &Path, template: &str) {
    if std::fs::create_dir_all(root).is_err() || root.join("config.yaml").is_file() {
        return;
    }
    // Solo en la carpeta de datos del modo instalado: en una carpeta portable el
    // config.yaml lo pone el usuario (o no hace falta).
    if root == data_dir() {
        let _ = std::fs::write(root.join("config.yaml"), first_config(template, &default_output_dir()));
    }
}

/// Busca un ejecutable en el PATH (con las extensiones de Windows).
pub fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let extensions: Vec<&str> = if cfg!(windows) { vec!["", ".exe", ".cmd", ".bat"] } else { vec![""] };
    for dir in std::env::split_paths(&path) {
        for ext in &extensions {
            let candidate = dir.join(format!("{name}{ext}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Falla pronto y con un mensaje util si falta el binario.
pub fn require_ffmpeg() -> Result<()> {
    let missing: Vec<&str> = ["ffmpeg", "ffprobe"].into_iter().filter(|b| which(b).is_none()).collect();
    if missing.is_empty() {
        return Ok(());
    }
    Err(FfmpegError(format!(
        "No se encontro {} en el PATH.\n  \
         Windows: doble clic en viralshorts.bat (lo instala solo)\n           \
         o bien: winget install Gyan.FFmpeg\n  \
         macOS:   brew install ffmpeg\n  \
         Linux:   sudo apt install ffmpeg\n\
         Comprueba luego con: ffmpeg -version",
        missing.join(" ni ")
    )))
}

/// Ejecuta un comando con tope de tiempo y devuelve su salida.
fn output_with_timeout(cmd: &mut Command, timeout: Duration) -> std::io::Result<Output> {
    let mut child = cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut buf);
            }
            buf
        })
    };
    let out = drain(child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    let err = drain(child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>));

    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "tiempo agotado"));
        }
        thread::sleep(Duration::from_millis(25));
    };
    Ok(Output {
        status,
        stdout: out.join().unwrap_or_default(),
        stderr: err.join().unwrap_or_default(),
    })
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

// ----------------------------------------------------------------------------
// Sondeo
// ----------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct MediaInfo {
    pub path: String,
    pub width: i64,
    pub height: i64,
    pub duration: f64,
    pub fps: f64,
    pub has_audio: bool,
    /// Proporcion del pixel. Casi siempre 1, pero hay grabaciones (DV, video
    /// anamorfico) con pixeles no cuadrados: ahi width/height NO es la
    /// proporcion con la que se ve el video, y usarla deforma los recortes.
    pub sar: f64,
}

impl MediaInfo {
    pub fn new(path: &str, width: i64, height: i64) -> Self {
        Self { path: path.into(), width, height, duration: 0.0, fps: 30.0, has_audio: true, sar: 1.0 }
    }

    /// Anchura con la que el video se ve realmente, en pixeles cuadrados.
    pub fn display_width(&self) -> i64 {
        even((self.width as f64 * self.sar).round_ties_even() as i64).max(2)
    }

    pub fn square_pixels(&self) -> bool {
        (self.sar - 1.0).abs() < 1e-3
    }

    pub fn aspect(&self) -> f64 {
        if self.height == 0 { 1.0 } else { self.display_width() as f64 / self.height as f64 }
    }
}

/// Los codecs 4:2:0 exigen dimensiones pares.
pub fn even(value: i64) -> i64 {
    value - value.rem_euclid(2)
}

/// Lee dimensiones, duracion, fps y presencia de audio de un fichero.
pub fn probe(path: &Path) -> Result<MediaInfo> {
    let output = command("ffprobe")
        .args(["-v", "error", "-print_format", "json", "-show_format", "-show_streams"])
        .arg(path)
        .output()
        .map_err(|e| FfmpegError(format!("No se pudo lanzar ffprobe: {e}")))?;
    if !output.status.success() {
        return Err(FfmpegError(format!(
            "ffprobe fallo con '{}':\n{}",
            path.display(),
            text(&output.stderr).trim()
        )));
    }
    let data: Value = serde_json::from_slice(&output.stdout)
        .map_err(|e| FfmpegError(format!("ffprobe devolvio algo ilegible ({e}).")))?;
    media_info_from_json(&path.display().to_string(), &data)
}

pub fn media_info_from_json(path: &str, data: &Value) -> Result<MediaInfo> {
    let streams: &[Value] = data["streams"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    let kind = |s: &Value| s["codec_type"].as_str().map(str::to_owned);
    let video = streams
        .iter()
        .find(|s| kind(s).as_deref() == Some("video"))
        .ok_or_else(|| FfmpegError(format!("'{path}' no contiene ninguna pista de video.")))?;
    let has_audio = streams.iter().any(|s| kind(s).as_deref() == Some("audio"));

    let duration = first_positive(&[&data["format"]["duration"], &video["duration"]])
        .ok_or_else(|| FfmpegError(format!("No se pudo determinar la duracion de '{path}'.")))?;

    let mut width = video["width"].as_i64().unwrap_or(0);
    let mut height = video["height"].as_i64().unwrap_or(0);
    let mut sar = parse_sar(video);
    // Algunos moviles graban en vertical con la rotacion en metadatos; ffprobe
    // da width/height del stream sin rotar, asi que se intercambian.
    if matches!(rotation_of(video), 90 | 270) {
        std::mem::swap(&mut width, &mut height);
        sar = if sar != 0.0 { 1.0 / sar } else { 1.0 };
    }

    Ok(MediaInfo { path: path.into(), width, height, duration, fps: parse_fps(video), has_audio, sar })
}

fn as_number(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn first_positive(values: &[&Value]) -> Option<f64> {
    values.iter().filter_map(|v| as_number(v)).find(|f| *f > 0.0 && f.is_finite())
}

/// Proporcion del pixel ('1:1', '4:3'...). 1.0 si no se declara.
fn parse_sar(stream: &Value) -> f64 {
    let raw = stream["sample_aspect_ratio"].as_str().unwrap_or("1:1");
    let mut parts = raw.split(':').map(|p| p.trim().parse::<i64>());
    match (parts.next(), parts.next(), parts.next()) {
        (Some(Ok(num)), Some(Ok(den)), None) if num > 0 && den > 0 => num as f64 / den as f64,
        _ => 1.0,
    }
}

fn rotation_of(stream: &Value) -> i64 {
    let mut rotation = as_number(&stream["tags"]["rotate"]).unwrap_or(0.0) as i64;
    if let Some(list) = stream["side_data_list"].as_array() {
        for side in list {
            if let Some(r) = as_number(&side["rotation"]) {
                rotation = r as i64;
            }
        }
    }
    rotation.abs().rem_euclid(360)
}

fn parse_fps(stream: &Value) -> f64 {
    for key in ["avg_frame_rate", "r_frame_rate"] {
        let Some(raw) = stream[key].as_str() else { continue };
        if raw == "0/0" {
            continue;
        }
        let Some((num, den)) = raw.split_once('/') else { continue };
        if let (Ok(num), Ok(den)) = (num.parse::<f64>(), den.parse::<f64>()) {
            if den != 0.0 && num / den > 0.0 {
                return num / den;
            }
        }
    }
    30.0
}

// ----------------------------------------------------------------------------
// Ejecucion
// ----------------------------------------------------------------------------

/// Ejecuta ffmpeg mostrando una barra de progreso. `total_duration` es la
/// duracion esperada de la SALIDA; sirve para calcular el porcentaje.
pub fn run(args: &[String], cwd: Option<&Path>, total_duration: Option<f64>, desc: &str) -> Result<()> {
    let mut full: Vec<String> = ["-hide_banner", "-loglevel", "error", "-nostdin", "-y", "-progress", "pipe:1", "-nostats"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    full.extend_from_slice(args);
    log::debug!("Ejecutando: ffmpeg {}", full.join(" "));

    let mut cmd = command("ffmpeg");
    cmd.args(&full).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    let mut child = cmd.spawn().map_err(|e| FfmpegError(format!("No se pudo lanzar ffmpeg: {e}")))?;

    // stderr en su propio hilo: si se llena mientras se lee stdout, ffmpeg se bloquea.
    let mut stderr_pipe = child.stderr.take();
    let stderr_reader = thread::spawn(move || {
        let mut buf = String::new();
        if let Some(pipe) = stderr_pipe.as_mut() {
            let _ = pipe.read_to_string(&mut buf);
        }
        buf
    });

    let show = total_duration.is_some_and(|d| d > 0.0) && progress::mode() != progress::Mode::Off;
    if let Some(stdout) = child.stdout.take() {
        for line in BufReader::new(stdout).lines().map_while(std::result::Result::ok) {
            if !show {
                continue;
            }
            if let Some(micros) = line.strip_prefix("out_time_us=").and_then(|v| v.trim().parse::<i64>().ok()) {
                progress::progress("render", micros as f64 / 1e6 / total_duration.unwrap_or(1.0), desc, false);
            }
        }
    }
    let status = child.wait().map_err(|e| FfmpegError(format!("ffmpeg no termino: {e}")))?;
    let stderr = stderr_reader.join().unwrap_or_default();
    if show {
        progress::progress("render", 1.0, desc, true);
    }

    if !status.success() {
        let tail: String = {
            let t = stderr.trim();
            let skip = t.chars().count().saturating_sub(4000);
            t.chars().skip(skip).collect()
        };
        return Err(FfmpegError(format!(
            "ffmpeg fallo (codigo {}).\n--- comando ---\nffmpeg {}\n--- stderr ---\n{tail}",
            status.code().map_or("?".into(), |c| c.to_string()),
            full.join(" ")
        )));
    }
    if !stderr.trim().is_empty() {
        log::debug!("ffmpeg stderr: {}", stderr.trim());
    }
    Ok(())
}

// ----------------------------------------------------------------------------
// Audio
// ----------------------------------------------------------------------------

/// Argumentos de ffmpeg para sacar un tramo de audio mono a `rate` Hz.
fn audio_args(path: &Path, start: Option<f64>, duration: Option<f64>, rate: u32) -> Vec<String> {
    let mut args: Vec<String> = ["-v", "error", "-nostdin"].map(String::from).to_vec();
    if let Some(start) = start {
        args.extend(["-ss".into(), format!("{start:.3}")]);
    }
    args.extend(["-i".into(), path.to_string_lossy().into_owned()]);
    if let Some(duration) = duration {
        args.extend(["-t".into(), format!("{duration:.3}")]);
    }
    args.extend(["-vn", "-ac", "1", "-ar"].map(String::from));
    args.push(rate.to_string());
    args
}

/// Decodifica el audio a mono de 16 bits y lo entrega por trozos de
/// `chunk_samples` muestras (float32 en [-1, 1]; el ultimo trozo puede ser mas
/// corto). Nunca tiene el audio entero en memoria: tres horas a 16 kHz son
/// 350 MB de PCM, y para medir energia no hace falta guardarlas.
pub fn decode_audio_chunks(
    path: &Path,
    rate: u32,
    start: Option<f64>,
    duration: Option<f64>,
    chunk_samples: usize,
    mut on_chunk: impl FnMut(&[f32]),
) -> Result<()> {
    let mut args = audio_args(path, start, duration, rate);
    args.extend(["-f", "s16le", "-"].map(String::from));

    let mut child = command("ffmpeg")
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| FfmpegError(format!("No se pudo lanzar ffmpeg: {e}")))?;
    let mut stderr_pipe = child.stderr.take();
    let stderr_reader = thread::spawn(move || {
        let mut buf = String::new();
        if let Some(pipe) = stderr_pipe.as_mut() {
            let _ = pipe.read_to_string(&mut buf);
        }
        buf
    });

    let mut stdout = child.stdout.take().expect("stdout encauzado");
    let mut bytes = vec![0u8; chunk_samples * 2];
    let mut samples = vec![0f32; chunk_samples];
    loop {
        // `read_exact` a trozos: un trozo completo, o el resto al final.
        let mut filled = 0;
        while filled < bytes.len() {
            match stdout.read(&mut bytes[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        let count = filled / 2;
        if count == 0 {
            break;
        }
        for (sample, pair) in samples.iter_mut().zip(bytes[..count * 2].chunks_exact(2)) {
            *sample = i16::from_le_bytes([pair[0], pair[1]]) as f32 / 32768.0;
        }
        on_chunk(&samples[..count]);
        if filled < bytes.len() {
            break;
        }
    }

    let status = child.wait().map_err(|e| FfmpegError(format!("ffmpeg no termino: {e}")))?;
    let stderr = stderr_reader.join().unwrap_or_default();
    if !status.success() {
        let t = stderr.trim();
        let skip = t.chars().count().saturating_sub(2000);
        return Err(FfmpegError(format!(
            "No se pudo decodificar el audio de '{}':\n{}",
            path.display(),
            t.chars().skip(skip).collect::<String>()
        )));
    }
    Ok(())
}

/// Guarda un tramo de audio como WAV mono de 16 bits (lo que pide whisper.cpp).
pub fn extract_wav(path: &Path, start: Option<f64>, duration: Option<f64>, rate: u32, output: &Path) -> Result<()> {
    let mut args = audio_args(path, start, duration, rate);
    args.extend(["-c:a", "pcm_s16le", "-y"].map(String::from));
    args.push(output.to_string_lossy().into_owned());

    let result = command("ffmpeg")
        .args(&args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| FfmpegError(format!("No se pudo lanzar ffmpeg: {e}")))?;
    if !result.status.success() {
        return Err(FfmpegError(format!(
            "No se pudo extraer el audio de '{}':\n{}",
            path.display(),
            text(&result.stderr).trim()
        )));
    }
    Ok(())
}

// ----------------------------------------------------------------------------
// Capacidades de la version instalada
// ----------------------------------------------------------------------------

/// Opciones de un filtro, a partir de la salida de `ffmpeg -h filter=<nombre>`.
/// Las lineas de opciones tienen la forma "   nombre  <tipo>  ...".
pub fn parse_filter_options(help: &str) -> HashSet<String> {
    help.lines()
        .filter(|line| line.starts_with("   ") && line.contains('<'))
        .filter_map(|line| line.split_whitespace().next())
        .map(str::to_owned)
        .collect()
}

/// Indica si el filtro acepta esa opcion en la version instalada. `crop` tenia
/// `eval` hasta ffmpeg 7 y en ffmpeg 8 lo elimino. Preguntarselo al binario es
/// mas fiable que parsear su version.
pub fn filter_supports(filter: &str, option: &str) -> bool {
    static CACHE: OnceLock<Mutex<HashMap<String, HashSet<String>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    let mut cache = cache.lock().unwrap();
    let options = cache.entry(filter.to_string()).or_insert_with(|| {
        command("ffmpeg")
            .args(["-hide_banner", "-h", &format!("filter={filter}")])
            .output()
            .map(|o| parse_filter_options(&text(&o.stdout)))
            .unwrap_or_default()
    });
    options.contains(option)
}

/// Escapa una ruta para usarla dentro de un filtergraph de ffmpeg.
pub fn escape_filter_path(path: &Path) -> String {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    absolute.to_string_lossy().replace('\\', "/").replace(':', "\\:").replace('\'', "\\'")
}

/// Indica si el binario acepta `-filter_complex_script`. ffmpeg 7.0 la marco
/// obsoleta en favor de `-/filter_complex` y despues de la 8.1 la elimino del
/// todo. Se pregunta al binario porque las compilaciones de desarrollo no
/// tienen un numero de version que valga.
pub fn supports_filter_complex_script() -> bool {
    static SUPPORTED: OnceLock<bool> = OnceLock::new();
    *SUPPORTED.get_or_init(|| {
        command("ffmpeg")
            .args(["-hide_banner", "-h", "full"])
            .output()
            .map(|o| text(&o.stdout).contains("filter_complex_script"))
            .unwrap_or(false)
    })
}

/// Guarda el filtergraph en fichero y devuelve los argumentos de ffmpeg. Pasar
/// el grafo por fichero evita el limite de longitud de la linea de comandos de
/// Windows y los problemas de escapado. La ruta es ABSOLUTA a proposito: ffmpeg
/// se lanza con cwd en la carpeta de trabajo, y una relativa se resolveria dos
/// veces contra esa misma carpeta.
pub fn write_filter_script(graph: &str, path: &Path) -> std::io::Result<Vec<String>> {
    write_filter_script_with(graph, path, supports_filter_complex_script())
}

pub fn write_filter_script_with(graph: &str, path: &Path, supported: bool) -> std::io::Result<Vec<String>> {
    std::fs::write(path, graph)?;
    let target = std::path::absolute(path)?.to_string_lossy().into_owned();
    let flag = if supported { "-filter_complex_script" } else { "-/filter_complex" };
    Ok(vec![flag.into(), target])
}

// ----------------------------------------------------------------------------
// Codificadores por hardware
// ----------------------------------------------------------------------------

/// Orden de preferencia: NVENC es el mas rapido y comun; QSV (Intel) y AMF
/// (AMD) van despues. El ultimo recurso siempre es libx264.
const HW_CANDIDATES: [&str; 3] = ["h264_nvenc", "h264_qsv", "h264_amf"];

/// Nombres de codificadores de video a partir de `ffmpeg -encoders`.
/// Formato: " V....D h264_nvenc   NVIDIA NVENC H.264 encoder".
pub fn parse_encoders(listing: &str) -> HashSet<String> {
    listing
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let flags = parts.next()?;
            let name = parts.next()?;
            flags.starts_with('V').then(|| name.to_owned())
        })
        .collect()
}

pub fn available_encoders() -> &'static HashSet<String> {
    static CACHE: OnceLock<HashSet<String>> = OnceLock::new();
    CACHE.get_or_init(|| {
        output_with_timeout(command("ffmpeg").args(["-hide_banner", "-encoders"]), Duration::from_secs(30))
            .map(|o| parse_encoders(&text(&o.stdout)))
            .unwrap_or_default()
    })
}

/// Codifica un fotograma de prueba: declararlo no es lo mismo que tenerlo.
/// ffmpeg lista h264_nvenc aunque no haya ninguna GPU NVIDIA.
pub fn encoder_works(name: &str) -> bool {
    static CACHE: OnceLock<Mutex<HashMap<String, bool>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    if let Some(&known) = cache.lock().unwrap().get(name) {
        return known;
    }
    let ok = available_encoders().contains(name)
        && output_with_timeout(
            command("ffmpeg").args([
                "-hide_banner", "-loglevel", "error", "-nostdin", "-y",
                "-f", "lavfi", "-i", "color=c=black:s=256x256:d=0.1",
                "-frames:v", "1", "-c:v", name, "-f", "null", "-",
            ]),
            Duration::from_secs(60),
        )
        .is_ok_and(|o| o.status.success());
    cache.lock().unwrap().insert(name.to_string(), ok);
    ok
}

/// Codificador por hardware utilizable, o None para usar libx264.
/// `preference`: "auto" prueba NVENC, QSV y AMF por ese orden; "off" no prueba
/// nada; cualquier otro valor se toma como el nombre exacto del codificador.
pub fn detect_hardware_encoder(preference: &str) -> Option<String> {
    detect_hardware_encoder_with(preference, encoder_works)
}

pub fn detect_hardware_encoder_with(preference: &str, works: impl Fn(&str) -> bool) -> Option<String> {
    let preference = preference.trim().to_lowercase();
    let preference = if preference.is_empty() { "auto".to_string() } else { preference };
    if ["off", "no", "false", "none", "libx264", "cpu"].contains(&preference.as_str()) {
        return None;
    }

    let candidates: Vec<&str> =
        if preference == "auto" { HW_CANDIDATES.to_vec() } else { vec![preference.as_str()] };
    for name in candidates {
        if works(name) {
            log::info!("Codificador por hardware disponible: {name}.");
            return Some(name.to_string());
        }
    }
    if preference != "auto" {
        log::warn!("El codificador '{preference}' no funciona en esta maquina; se usara libx264.");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -- sondeo --------------------------------------------------------------

    fn sonda(video: Value) -> Value {
        json!({"format": {"duration": "61.5"}, "streams": [video, {"codec_type": "audio"}]})
    }

    #[test]
    fn video_normal() {
        let data = sonda(json!({"codec_type": "video", "width": 1920, "height": 1080,
                                "avg_frame_rate": "30000/1001", "sample_aspect_ratio": "1:1"}));
        let info = media_info_from_json("v.mp4", &data).unwrap();
        assert_eq!((info.width, info.height), (1920, 1080));
        assert_eq!(info.duration, 61.5);
        assert!(info.has_audio && info.square_pixels());
        assert!((info.fps - 29.97).abs() < 0.01);
    }

    #[test]
    fn pixeles_no_cuadrados() {
        let data = sonda(json!({"codec_type": "video", "width": 720, "height": 576,
                                "sample_aspect_ratio": "16:11"}));
        let info = media_info_from_json("dv.mp4", &data).unwrap();
        assert!(!info.square_pixels());
        assert_eq!(info.display_width(), 1046);
        assert_eq!(info.display_width() % 2, 0);
    }

    #[test]
    fn rotacion_de_movil_intercambia_las_dimensiones() {
        for rotacion in [json!({"tags": {"rotate": "90"}}), json!({"side_data_list": [{"rotation": -90}]})] {
            let mut video = json!({"codec_type": "video", "width": 1920, "height": 1080});
            video.as_object_mut().unwrap().extend(rotacion.as_object().unwrap().clone());
            let info = media_info_from_json("m.mp4", &sonda(video)).unwrap();
            assert_eq!((info.width, info.height), (1080, 1920));
        }
    }

    #[test]
    fn sin_video_o_sin_duracion() {
        let solo_audio = json!({"format": {"duration": "10"}, "streams": [{"codec_type": "audio"}]});
        assert!(media_info_from_json("a.mp3", &solo_audio).is_err());
        let sin_duracion = json!({"format": {}, "streams": [{"codec_type": "video", "width": 2, "height": 2}]});
        assert!(media_info_from_json("v.mp4", &sin_duracion).is_err());
    }

    #[test]
    fn duracion_del_stream_si_el_formato_no_la_trae() {
        let data = json!({"format": {"duration": "N/A"},
                          "streams": [{"codec_type": "video", "width": 2, "height": 2, "duration": "7.25"}]});
        assert_eq!(media_info_from_json("v.mkv", &data).unwrap().duration, 7.25);
    }

    #[test]
    fn fps_y_sar_defectuosos() {
        assert_eq!(parse_fps(&json!({"avg_frame_rate": "0/0", "r_frame_rate": "25/1"})), 25.0);
        assert_eq!(parse_fps(&json!({})), 30.0);
        assert_eq!(parse_fps(&json!({"avg_frame_rate": "5/0"})), 30.0);
        assert_eq!(parse_sar(&json!({"sample_aspect_ratio": "0:1"})), 1.0);
        assert_eq!(parse_sar(&json!({"sample_aspect_ratio": "raro"})), 1.0);
        assert_eq!(parse_sar(&json!({"sample_aspect_ratio": "4:3"})), 4.0 / 3.0);
    }

    #[test]
    fn dimensiones_pares() {
        assert_eq!(even(961), 960);
        assert_eq!(even(960), 960);
        assert_eq!(MediaInfo::new("x", 3, 3).display_width(), 2);
    }

    // -- capacidades ---------------------------------------------------------

    #[test]
    fn opciones_de_un_filtro() {
        let ayuda = "Filter crop\n  Crop the input video.\n    slice threading supported\n\
                     crop AVOptions:\n   out_w             <string>     ..FV....... set the width crop area expression\n\
                     \x20  eval              <int>        ..FV....... specify when to evaluate expressions (from 0 to 1)\n\
                     \x20    init                         ..FV....... eval expressions once during initialization\n";
        let opciones = parse_filter_options(ayuda);
        assert!(opciones.contains("out_w"));
        assert!(opciones.contains("eval"));
        assert!(!opciones.contains("init"), "los valores de un enum no son opciones");
        assert!(parse_filter_options("").is_empty());
    }

    #[test]
    fn el_grafo_se_escribe_con_la_opcion_que_toca() {
        let dir = tempfile::tempdir().unwrap();
        let destino = dir.path().join("grafo.filter");
        for (soporta, esperado) in [(true, "-filter_complex_script"), (false, "-/filter_complex")] {
            let args = write_filter_script_with("nullsrc[vout]", &destino, soporta).unwrap();
            assert_eq!(args[0], esperado);
            assert!(Path::new(&args[1]).is_absolute());
            assert_eq!(std::fs::read_to_string(&destino).unwrap(), "nullsrc[vout]");
        }
    }

    #[test]
    fn rutas_en_filtros() {
        let escapada = escape_filter_path(Path::new("C:\\fuentes\\it's"));
        assert!(!escapada.contains('\\') || escapada.contains("\\:") || escapada.contains("\\'"));
        assert!(escapada.contains("\\'"));
        assert!(!escapada.replace("\\:", "").contains(':'));
    }

    // -- codificadores -------------------------------------------------------

    #[test]
    fn lista_de_codificadores() {
        let salida = "Encoders:\n V..... = Video\n ------\n V....D h264_nvenc           NVIDIA NVENC H.264 encoder\n \
                      V....D libx264              libx264 H.264\n A....D aac                  AAC\n S..... srt  subs\n";
        let nombres = parse_encoders(salida);
        assert!(nombres.contains("h264_nvenc") && nombres.contains("libx264"));
        assert!(!nombres.contains("aac") && !nombres.contains("srt"));
    }

    #[test]
    fn auto_prueba_en_orden_y_se_queda_con_el_primero_que_funciona() {
        let visto = std::cell::RefCell::new(Vec::new());
        let elegido = detect_hardware_encoder_with("auto", |n| {
            visto.borrow_mut().push(n.to_string());
            n == "h264_qsv"
        });
        assert_eq!(elegido.as_deref(), Some("h264_qsv"));
        assert_eq!(*visto.borrow(), ["h264_nvenc", "h264_qsv"]);
    }

    #[test]
    fn sin_hardware_cae_a_cpu() {
        assert_eq!(detect_hardware_encoder_with("auto", |_| false), None);
        assert_eq!(detect_hardware_encoder_with("h264_nvenc", |_| false), None);
    }

    #[test]
    fn off_no_prueba_nada() {
        for p in ["off", "OFF", "cpu", "libx264", "none"] {
            assert_eq!(detect_hardware_encoder_with(p, |_| panic!("no debe probar")), None, "{p}");
        }
    }

    #[test]
    fn un_nombre_concreto_se_respeta() {
        assert_eq!(detect_hardware_encoder_with("h264_amf", |n| n == "h264_amf").as_deref(), Some("h264_amf"));
        assert_eq!(detect_hardware_encoder_with("", |n| n == "h264_nvenc").as_deref(), Some("h264_nvenc"));
    }
}

#[cfg(test)]
mod tests_config_inicial {
    use super::*;
    use crate::config::Config;

    fn salida() -> PathBuf {
        PathBuf::from(r"C:\Users\yo\Videos\viralshorts")
    }

    fn cargar(texto: &str) -> Config {
        let mut cfg = Config::default();
        cfg.merge_yaml(texto, "c.yaml").expect("el YAML resultante debe ser valido");
        cfg
    }

    #[test]
    fn la_carpeta_de_salida_se_cambia_con_fin_de_linea_de_windows_o_unix() {
        let plantilla = "clips:\n  num_clips: 5\n\noutput:\n  dir: output\n  crf: 20\npaths:\n  work_dir: work\n";
        for texto in [plantilla.to_string(), plantilla.replace('\n', "\r\n")] {
            let nuevo = first_config(&texto, &salida());
            assert_eq!(nuevo.matches("output:").count(), 1, "{nuevo}");
            let cfg = cargar(&nuevo);
            assert_eq!(cfg.output.dir, "C:/Users/yo/Videos/viralshorts");
            assert_eq!((cfg.output.crf, cfg.clips.num_clips, cfg.paths.work_dir.as_str()), (20, 5, "work"));
        }
    }

    #[test]
    fn sin_linea_dir_o_sin_seccion_se_anade_sin_duplicar() {
        let sin_dir = first_config("output:\n  crf: 18\n", &salida());
        assert_eq!(cargar(&sin_dir).output.dir, "C:/Users/yo/Videos/viralshorts");
        assert_eq!(cargar(&sin_dir).output.crf, 18);

        let sin_seccion = first_config("clips:\n  num_clips: 2\n", &salida());
        assert_eq!(cargar(&sin_seccion).output.dir, "C:/Users/yo/Videos/viralshorts");
        assert_eq!(sin_seccion.matches("output:").count(), 1);
        assert_eq!(cargar(&first_config("", &salida())).output.dir, "C:/Users/yo/Videos/viralshorts");
    }

    #[test]
    fn el_config_del_proyecto_sirve_de_plantilla_tal_cual() {
        let plantilla = include_str!("../../config.yaml");
        let nuevo = first_config(plantilla, &salida());
        assert_eq!(nuevo.matches("\noutput:").count(), 1);
        let cfg = cargar(&nuevo);
        assert_eq!(cfg.output.dir, "C:/Users/yo/Videos/viralshorts");
        assert_eq!(cfg.clips.max_duration, 45.0, "lo demas se conserva");
    }

    #[test]
    fn solo_se_escribe_la_primera_vez_y_solo_en_la_carpeta_de_datos() {
        let dir = tempfile::tempdir().unwrap();
        ensure_home(dir.path(), "clips:\n  num_clips: 7\n");
        assert!(!dir.path().join("config.yaml").exists(), "una carpeta portable no se toca");
    }
}
