//! Transcripcion con whisper.cpp: UNA sola pasada para todo el pipeline.
//!
//! El resultado (palabras con marca de tiempo individual) se usa para dos cosas
//! a la vez: elegir los momentos del podcast y generar los subtitulos.
//!
//! El motor es el ejecutable `whisper-cli` de whisper.cpp, que descarga el
//! instalador. Hay dos: el de CPU y, si el equipo tiene una NVIDIA, el de CUDA.
//! Si el de CUDA falla a mitad se repite entero en CPU, igual que hacia la
//! version Python con faster-whisper.
//!
//! El resultado se cachea en disco: volver a lanzar el pipeline sobre el mismo
//! video con la misma configuracion no vuelve a transcribir.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Instant;

use serde_json::Value;
use sha1::{Digest, Sha1};

use crate::config::TranscribeConfig;
use crate::ffmpeg::{self, command};
use crate::progress;
use crate::words::{Segment, Transcript, Word};

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct TranscribeError(pub String);

type Result<T> = std::result::Result<T, TranscribeError>;

// ----------------------------------------------------------------------------
// Motor
// ----------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Device {
    Cpu,
    Cuda,
}

#[derive(Debug, Clone)]
pub struct EngineOptions {
    /// None = autodetectar.
    pub language: Option<String>,
    pub model_size: String,
    pub beam_size: i64,
    pub threads: usize,
    /// Segundo del video donde empieza el audio que se le pasa: los tiempos que
    /// whisper imprime son relativos a ese trozo.
    pub offset: f64,
    /// Texto inicial (vocabulario) para orientar a whisper.
    pub prompt: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct EngineOutput {
    pub language: Option<String>,
    /// Con tiempos relativos al inicio del audio.
    pub segments: Vec<Segment>,
}

/// Lo que el transcriptor necesita de un motor. Es un trait para poder probar
/// la logica de reintento y de cache sin un modelo de verdad.
pub trait Engine {
    fn cuda_available(&self) -> bool;
    fn transcribe(
        &mut self,
        wav: &Path,
        device: Device,
        options: &EngineOptions,
        on_progress: &mut dyn FnMut(f64),
    ) -> std::result::Result<EngineOutput, TranscribeError>;
}

// Mensajes con los que un motor avisa de que la GPU no se puede usar. La
// comparacion es por texto porque ni whisper.cpp ni CUDA distinguen el fallo
// con un tipo propio.
const GPU_FAILURES: [&str; 8] = [
    "cublas",
    "cudnn",
    "cuda",
    "gpu",
    "libcu",
    "is not found or cannot be loaded",
    "no kernel image is available",
    "out of memory",
];

pub fn is_gpu_failure(error: &str) -> bool {
    let lowered = error.to_lowercase();
    GPU_FAILURES.iter().any(|mark| lowered.contains(mark))
}

pub struct Transcriber {
    cfg: TranscribeConfig,
    cache_dir: PathBuf,
    engine: Box<dyn Engine>,
    /// Se pone a true cuando la GPU ya ha fallado una vez: a partir de ahi no
    /// se vuelve a intentar en esta ejecucion.
    cpu_only: bool,
    /// Inicio (en el video) del trozo que se esta transcribiendo.
    offset: f64,
}

impl Transcriber {
    pub fn new(cfg: TranscribeConfig, cache_dir: impl Into<PathBuf>, engine: Box<dyn Engine>) -> Result<Self> {
        let cache_dir = cache_dir.into();
        std::fs::create_dir_all(&cache_dir)
            .map_err(|e| TranscribeError(format!("No se pudo crear {}: {e}", cache_dir.display())))?;
        Ok(Self { cfg, cache_dir, engine, cpu_only: false, offset: 0.0 })
    }

    fn resolve_device(&self) -> Device {
        if self.cpu_only {
            return Device::Cpu;
        }
        match self.cfg.device.as_str() {
            "cpu" => Device::Cpu,
            "cuda" => Device::Cuda,
            _ if self.engine.cuda_available() => Device::Cuda,
            _ => Device::Cpu,
        }
    }

    /// Hilos de CPU. Se usan los nucleos fisicos: pasar de ahi (hyperthreading)
    /// no acelera la inferencia y calienta de mas.
    fn threads(&self) -> usize {
        if self.cfg.cpu_threads > 0 {
            return self.cfg.cpu_threads as usize;
        }
        let logical = std::thread::available_parallelism().map_or(4, |n| n.get());
        if logical > 4 { (logical / 2).max(1) } else { logical }
    }

    fn options(&self) -> EngineOptions {
        EngineOptions {
            language: self.cfg.language.clone().filter(|l| !l.is_empty() && l != "auto"),
            model_size: self.cfg.model_size.clone(),
            beam_size: self.cfg.beam_size,
            threads: self.threads(),
            offset: self.offset,
            prompt: vocabulary_prompt(&self.cfg.initial_prompt),
        }
    }

    /// Transcribe, y si la GPU falla a mitad, repite entero en CPU. Hace falta
    /// reintentar aqui y no solo al cargar: en un equipo con GPU pero sin las
    /// librerias CUDA el motor arranca sin una queja y el error sale ya
    /// empezada la transcripcion.
    pub fn run(&mut self, wav: &Path, on_progress: &mut dyn FnMut(f64)) -> Result<EngineOutput> {
        let options = self.options();
        let device = self.resolve_device();
        match self.engine.transcribe(wav, device, &options, on_progress) {
            Err(error) if device == Device::Cuda && is_gpu_failure(&error.0) => {
                log::warn!("La GPU no se pudo usar para transcribir ({error}).");
                log::warn!(
                    "Se sigue con el procesador: tarda mas, pero termina. Para arreglar la GPU: 'install\\Activar GPU NVIDIA.bat'."
                );
                self.cpu_only = true;
                self.engine.transcribe(wav, Device::Cpu, &options, on_progress)
            }
            other => other,
        }
    }

    // -- transcripcion --------------------------------------------------------

    /// Transcribe el video entero.
    pub fn transcribe(&mut self, path: &Path, duration: f64) -> Result<Transcript> {
        let (cache_path, cached) = self.cached(path, None)?;
        if let Some(cached) = cached {
            return Ok(cached);
        }

        log::info!("Transcribiendo {} ({})...", file_name(path), fmt_duration(duration));
        let started = Instant::now();
        let output = self.transcribe_range(path, None, duration, 0.0, duration)?;
        progress_step(duration, duration, true);

        let transcript = self.finish(output.segments, output.language, duration, path)?;
        log_summary(&transcript, started, duration, duration);
        write_cache(&cache_path, &transcript);
        Ok(transcript)
    }

    /// Transcribe solo los tramos indicados, con tiempos absolutos. El
    /// resultado se comporta como una transcripcion normal salvo que entre zona
    /// y zona no hay palabras: los huecos son justamente las partes del podcast
    /// que nadie va a recortar.
    pub fn transcribe_regions(&mut self, path: &Path, duration: f64, regions: &[(f64, f64)]) -> Result<Transcript> {
        let regions: Vec<(f64, f64)> = regions.iter().copied().filter(|(a, b)| b - a > 0.5).collect();
        if regions.is_empty() {
            return self.transcribe(path, duration);
        }

        let (cache_path, cached) = self.cached(path, Some(&regions))?;
        if let Some(cached) = cached {
            return Ok(cached);
        }

        let covered: f64 = regions.iter().map(|(a, b)| b - a).sum();
        log::info!(
            "Transcribiendo {} zonas de {} ({} en total, no {})...",
            regions.len(),
            file_name(path),
            fmt_duration(covered),
            fmt_duration(duration)
        );
        let started = Instant::now();

        let mut segments: Vec<Segment> = Vec::new();
        let mut language: Option<String> = None;
        let mut done = 0.0;
        for &(start, end) in &regions {
            let output = self.transcribe_range(path, Some(start), end - start, done, covered)?;
            language = language.or(output.language);
            segments.extend(output.segments.into_iter().map(|s| shift(s, start)));
            done += end - start;
            progress_step(done, covered, false);
        }
        progress_step(covered, covered, true);

        segments.sort_by(|a, b| a.start.total_cmp(&b.start));
        let transcript = self.finish(segments, language, duration, path)?;
        log_summary(&transcript, started, covered, duration);
        write_cache(&cache_path, &transcript);
        Ok(transcript)
    }

    /// Un tramo de audio: se decodifica solo ese tramo (ffmpeg busca en el
    /// fichero, no lee las tres horas) y se le pasa al motor como WAV.
    fn transcribe_range(&mut self, path: &Path, start: Option<f64>, span: f64, done_before: f64, total: f64) -> Result<EngineOutput> {
        let wav = self.cache_dir.join(format!("tmp-{}-{}.wav", std::process::id(), start.map_or(0, |s| (s * 10.0) as i64)));
        ffmpeg::extract_wav(path, start, Some(span), 16_000, &wav).map_err(|e| TranscribeError(e.to_string()))?;
        self.offset = start.unwrap_or(0.0);
        let result = self.run(&wav, &mut |fraction| progress_step(done_before + fraction * span, total, false));
        let _ = std::fs::remove_file(&wav);
        result
    }

    fn finish(&self, segments: Vec<Segment>, language: Option<String>, duration: f64, path: &Path) -> Result<Transcript> {
        let mut segments = drop_repetition_loops(segments);
        let corrections = Corrections::parse(&self.cfg.corrections);
        if !corrections.is_empty() {
            for segment in &mut segments {
                corrections.apply_segment(segment);
            }
        }
        if segments.is_empty() {
            return Err(TranscribeError(format!(
                "La transcripcion de '{}' no devolvio ninguna palabra. Comprueba que el video tenga audio audible.",
                file_name(path)
            )));
        }
        Ok(Transcript {
            language: language.or_else(|| self.cfg.language.clone()).unwrap_or_else(|| "??".into()),
            duration,
            segments,
        })
    }

    /// Transcripcion guardada de `path`. Se busca primero por el video
    /// descargado (su enlace) y despues por el fichero, que es como se
    /// guardaban antes: asi no se pierden las que ya habia.
    fn cached(&self, path: &Path, regions: Option<&[(f64, f64)]>) -> Result<(PathBuf, Option<Transcript>)> {
        let primary = self.cache_dir.join(format!("{}.json", self.cache_key(path, regions, true)?));
        if let Some(found) = read_cache(&primary) {
            return Ok((primary, Some(found)));
        }
        let legacy = self.cache_dir.join(format!("{}.json", self.cache_key(path, regions, false)?));
        let found = (legacy != primary).then(|| read_cache(&legacy)).flatten();
        if let Some(found) = &found {
            write_cache(&primary, found);
        }
        Ok((primary, found))
    }

    /// Identifica la transcripcion por el video Y por como se pidio. Zonas
    /// distintas = transcripcion distinta: no pueden compartir entrada. El
    /// video es su enlace si es una descarga (sigue valiendo aunque se borre y
    /// se vuelva a bajar) y si no, el fichero con su tamano y su fecha.
    fn cache_key(&self, path: &Path, regions: Option<&[(f64, f64)]>, by_link: bool) -> Result<String> {
        let meta = std::fs::metadata(path).map_err(|e| TranscribeError(format!("No se pudo leer {}: {e}", path.display())))?;
        let video = match crate::download::identity(path).filter(|_| by_link) {
            Some(link) => format!("enlace:{link}"),
            None => {
                let mtime = meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
                let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
                format!("{}|{}|{mtime}", resolved.display(), meta.len())
            }
        };
        let zones = match regions {
            Some(r) => r.iter().map(|(a, b)| format!("{a:.1}-{b:.1}")).collect::<Vec<_>>().join("|"),
            None => "full".into(),
        };
        let payload = [
            "whispercpp".to_string(),
            self.cfg.initial_prompt.clone(),
            self.cfg.corrections.clone(),
            video,
            self.cfg.model_size.clone(),
            self.cfg.language.clone().unwrap_or_else(|| "None".into()),
            self.cfg.beam_size.to_string(),
            zones,
        ]
        .join("|");
        let digest = Sha1::digest(payload.as_bytes());
        Ok(format!("{digest:x}")[..16].to_string())
    }
}

/// Frase de arranque para whisper a partir del vocabulario del usuario.
fn vocabulary_prompt(vocabulary: &str) -> Option<String> {
    let clean: String = vocabulary.chars().filter(|c| !c.is_control()).collect::<String>().trim().to_string();
    (!clean.is_empty()).then(|| format!("Nombres y términos de la conversación: {}.", clean.trim_end_matches(['.', ',', ';'])))
}

/// Correcciones fijas `mal=bien;otra mal=otra bien`. Se buscan palabra a palabra
/// (sin importar mayusculas ni la puntuacion pegada) y pueden ser de varias palabras.
#[derive(Debug, Default, Clone)]
pub struct Corrections {
    /// (palabras a buscar en minuscula, sustitucion)
    rules: Vec<(Vec<String>, String)>,
}

fn core_of(word: &str) -> String {
    word.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase()
}

impl Corrections {
    pub fn parse(text: &str) -> Self {
        let rules = text
            .split(|c| c == ';' || c == '\n')
            .filter_map(|item| {
                let (bad, good) = item.split_once('=')?;
                let words: Vec<String> = bad.split_whitespace().map(core_of).filter(|w| !w.is_empty()).collect();
                let good = good.trim().to_string();
                (!words.is_empty() && !good.is_empty()).then_some((words, good))
            })
            .collect();
        Self { rules }
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Aplica las reglas a una lista de palabras. Conserva la puntuacion de
    /// delante y de detras, y el tiempo de la primera a la ultima palabra.
    pub fn apply_words(&self, words: Vec<Word>) -> Vec<Word> {
        let mut out: Vec<Word> = Vec::with_capacity(words.len());
        let mut i = 0;
        'next: while i < words.len() {
            for (pattern, good) in &self.rules {
                let end = i + pattern.len();
                if end <= words.len() && words[i..end].iter().zip(pattern).all(|(w, p)| &core_of(&w.text) == p) {
                    let first = &words[i];
                    let last = &words[end - 1];
                    let lead: String = first.text.chars().take_while(|c| !c.is_alphanumeric()).collect();
                    let trail: String = {
                        let mut tail: Vec<char> = last.text.chars().rev().take_while(|c| !c.is_alphanumeric()).collect();
                        tail.reverse();
                        tail.into_iter().collect()
                    };
                    out.push(Word { text: format!("{lead}{good}{trail}"), start: first.start, end: last.end, probability: 1.0 });
                    i = end;
                    continue 'next;
                }
            }
            out.push(words[i].clone());
            i += 1;
        }
        out
    }

    pub fn apply_segment(&self, segment: &mut Segment) {
        let words = std::mem::take(&mut segment.words);
        let before = words.len();
        segment.words = self.apply_words(words);
        if !segment.words.is_empty() && (before != segment.words.len() || !segment.text.is_empty()) {
            segment.text = segment.words.iter().map(|w| w.text.as_str()).collect::<Vec<_>>().join(" ");
        }
    }
}

fn shift(mut segment: Segment, offset: f64) -> Segment {
    segment.start += offset;
    segment.end += offset;
    segment.words = segment.words.into_iter().map(|w| w.shifted(offset)).collect();
    segment
}

fn read_cache(path: &Path) -> Option<Transcript> {
    if !path.is_file() {
        return None;
    }
    match std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str::<Transcript>(&t).ok()) {
        Some(mut transcript) => {
            log::info!("Transcripcion en cache: {}", file_name(path));
            // Las guardadas antes del filtro de bucles tambien se limpian.
            transcript.segments = drop_repetition_loops(transcript.segments);
            Some(transcript)
        }
        None => {
            log::warn!("Cache corrupta ({}); se vuelve a transcribir.", file_name(path));
            None
        }
    }
}

fn write_cache(path: &Path, transcript: &Transcript) {
    if let Err(error) = serde_json::to_string(transcript).map_err(|e| e.to_string()).and_then(|t| std::fs::write(path, t).map_err(|e| e.to_string())) {
        log::warn!("No se pudo guardar la cache de la transcripcion ({error}).");
    }
}

fn log_summary(transcript: &Transcript, started: Instant, covered: f64, duration: f64) {
    let elapsed = started.elapsed().as_secs_f64();
    log::info!(
        "Transcritas {} palabras en {:.0}s ({:.1}x tiempo real sobre lo transcrito, {:.0}x sobre el video). Idioma: {}",
        transcript.iter_words().count(),
        elapsed,
        if elapsed > 0.0 { covered / elapsed } else { 0.0 },
        if elapsed > 0.0 { duration / elapsed } else { 0.0 },
        transcript.language
    );
}

fn progress_step(current: f64, total: f64, last: bool) {
    if total > 0.0 {
        progress::progress("transcribe", current / total, "transcribiendo", last);
    }
}

fn file_name(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

fn fmt_duration(seconds: f64) -> String {
    let total = seconds as i64;
    let (minutes, secs) = (total / 60, total % 60);
    let (hours, minutes) = (minutes / 60, minutes % 60);
    if hours > 0 { format!("{hours}h{minutes:02}m{secs:02}s") } else { format!("{minutes}m{secs:02}s") }
}

// ----------------------------------------------------------------------------
// whisper.cpp
// ----------------------------------------------------------------------------

/// STATUS_DLL_NOT_FOUND: Windows no pudo cargar una DLL del ejecutable.
const DLL_NOT_FOUND: i32 = 0xC000_0135_u32 as i32;

pub struct WhisperCpp {
    cpu_exe: Option<PathBuf>,
    cuda_exe: Option<PathBuf>,
    models_dir: PathBuf,
    warned_missing_cuda: bool,
}

impl WhisperCpp {
    /// Busca los ejecutables: `bin/whisper` (CPU) y `bin/whisper-cuda`, o
    /// `whisper-cli` en el PATH.
    pub fn discover(root: &Path) -> Result<Self> {
        let exe = if cfg!(windows) { "whisper-cli.exe" } else { "whisper-cli" };
        let local = |dir: &str| Some(root.join("bin").join(dir).join(exe)).filter(|p| p.is_file());
        let cpu_exe = local("whisper").or_else(|| ffmpeg::which("whisper-cli"));
        let cuda_exe = local("whisper-cuda");
        if cpu_exe.is_none() && cuda_exe.is_none() {
            return Err(TranscribeError(
                "Falta whisper.cpp (whisper-cli). Doble clic en viralshorts.bat lo instala solo.".into(),
            ));
        }
        Ok(Self { cpu_exe, cuda_exe, models_dir: root.join("bin").join("models"), warned_missing_cuda: false })
    }

    pub fn model_path(&self, size: &str) -> PathBuf {
        self.models_dir.join(format!("ggml-{size}.bin"))
    }

    /// El modelo, descargandolo la primera vez.
    fn ensure_model(&self, size: &str) -> Result<PathBuf> {
        let path = self.model_path(size);
        if std::fs::metadata(&path).is_ok_and(|m| m.len() > 0) {
            return Ok(path);
        }
        let url = crate::setup::model_url(size);
        crate::fetch::download_file(&url, &path, &format!("el modelo de voz '{size}'"), "modelo")
            .map_err(|e| TranscribeError(e.to_string()))?;
        Ok(path)
    }
}

impl Engine for WhisperCpp {
    fn cuda_available(&self) -> bool {
        let nvidia = ffmpeg::which("nvidia-smi").is_some_and(|smi| {
            command(smi).arg("-L").stdin(Stdio::null()).output().is_ok_and(|o| o.status.success() && !o.stdout.is_empty())
        });
        if nvidia && self.cuda_exe.is_none() {
            if !self.warned_missing_cuda {
                log::info!(
                    "Hay GPU NVIDIA pero falta el motor CUDA de whisper.cpp; se transcribe con el procesador. \
                     Para usar la tarjeta: doble clic en 'install\\Activar GPU NVIDIA.bat'."
                );
            }
            return false;
        }
        nvidia
    }

    fn transcribe(
        &mut self,
        wav: &Path,
        device: Device,
        options: &EngineOptions,
        on_progress: &mut dyn FnMut(f64),
    ) -> Result<EngineOutput> {
        let exe = match device {
            Device::Cuda => self.cuda_exe.clone(),
            Device::Cpu => self.cpu_exe.clone().or_else(|| self.cuda_exe.clone()),
        }
        .ok_or_else(|| TranscribeError("No hay ejecutable de whisper.cpp para este dispositivo.".into()))?;
        let model = self.ensure_model(&options.model_size)?;

        let out_base = wav.with_extension("");
        let json_path = PathBuf::from(format!("{}.json", out_base.display()));
        let _ = std::fs::remove_file(&json_path);

        let mut cmd = command(&exe);
        cmd.arg("-m").arg(&model).arg("-f").arg(wav).arg("-of").arg(&out_base);
        // -sns: sin tokens de "no habla" ([Musica], aplausos...) que luego habria que filtrar.
        // Sin -np whisper escribe cada frase por stdout segun la reconoce: asi la
        // interfaz la puede ensenar en directo.
        cmd.args(["-ojf", "-pp", "-sns"]);
        cmd.args(["-l", options.language.as_deref().unwrap_or("auto")]);
        cmd.args(["-bs", &options.beam_size.max(1).to_string()]);
        cmd.args(["-t", &options.threads.max(1).to_string()]);
        // Sin arrastrar el texto anterior como contexto: es lento y en
        // podcasts largos provoca bucles de repeticion.
        cmd.args(["-mc", "0"]);
        if let Some(prompt) = options.prompt.as_deref() {
            // Con --carry-initial-prompt el vocabulario vale en todo el audio y no
            // solo en la primera ventana (sin contexto previo: -mc 0).
            cmd.args(["--prompt", prompt, "--carry-initial-prompt"]);
        }
        if device == Device::Cpu && self.cpu_exe.as_ref() != Some(&exe) {
            cmd.arg("-ng"); // motor CUDA usado como CPU
        }
        cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        log::debug!("Ejecutando: {cmd:?}");

        let mut child = cmd.spawn().map_err(|e| TranscribeError(format!("No se pudo lanzar {}: {e}", exe.display())))?;
        // stdout en su hilo: con el tubo sin vaciar, whisper se bloquearia.
        let live = child.stdout.take().map(|out| {
            let offset = options.offset;
            std::thread::spawn(move || {
                for line in BufReader::new(out).lines().map_while(std::result::Result::ok) {
                    if let Some((start, text)) = parse_live_line(&line) {
                        crate::progress::result("texto", serde_json::json!({"t": offset + start, "texto": text}));
                    }
                }
            })
        });
        let mut tail: Vec<String> = Vec::new();
        if let Some(stderr) = child.stderr.take() {
            for line in BufReader::new(stderr).lines().map_while(std::result::Result::ok) {
                if let Some(fraction) = parse_progress(&line) {
                    on_progress(fraction);
                } else if !line.trim().is_empty() {
                    tail.push(line);
                    if tail.len() > 30 {
                        tail.remove(0);
                    }
                }
            }
        }
        let status = child.wait().map_err(|e| TranscribeError(format!("whisper-cli no termino: {e}")))?;
        if let Some(handle) = live {
            let _ = handle.join();
        }
        if !status.success() {
            let detail = if status.code() == Some(DLL_NOT_FOUND) {
                "una DLL de CUDA is not found or cannot be loaded".to_string()
            } else {
                tail.join("\n")
            };
            return Err(TranscribeError(format!("whisper-cli fallo (codigo {}): {detail}", status.code().unwrap_or(-1))));
        }

        let text = std::fs::read_to_string(&json_path)
            .map_err(|e| TranscribeError(format!("whisper-cli no dejo resultado en {}: {e}", json_path.display())))?;
        let _ = std::fs::remove_file(&json_path);
        parse_whisper_json(&text)
    }
}

/// Una frase que whisper.cpp escribe por stdout mientras transcribe:
/// `[00:01:02.500 --> 00:01:05.000]   Hola, esto es una prueba.` -> (62.5, "Hola, ...").
pub fn parse_live_line(line: &str) -> Option<(f64, String)> {
    let rest = line.trim().strip_prefix('[')?;
    let (stamps, text) = rest.split_once(']')?;
    let (from, _) = stamps.split_once("-->")?;
    let mut seconds = 0.0;
    for part in from.trim().split(':') {
        seconds = seconds * 60.0 + part.trim().parse::<f64>().ok()?;
    }
    let text = text.trim();
    (!is_annotation(text) && !text.is_empty()).then(|| (seconds, text.to_string()))
}

/// "whisper_print_progress_callback: progress =  45%" -> 0.45
pub fn parse_progress(line: &str) -> Option<f64> {
    let rest = line.split("progress =").nth(1)?;
    let percent: f64 = rest.trim().trim_end_matches('%').trim().parse().ok()?;
    Some((percent / 100.0).clamp(0.0, 1.0))
}

/// Convierte el JSON completo de whisper.cpp (`-ojf`) en segmentos con palabras.
pub fn parse_whisper_json(text: &str) -> Result<EngineOutput> {
    let data: Value = serde_json::from_str(text).map_err(|e| TranscribeError(format!("Resultado de whisper.cpp ilegible: {e}")))?;
    let language = data["result"]["language"].as_str().filter(|l| !l.is_empty()).map(str::to_string);

    let mut segments = Vec::new();
    for item in data["transcription"].as_array().map(Vec::as_slice).unwrap_or(&[]) {
        let seg_start = item["offsets"]["from"].as_f64().unwrap_or(0.0) / 1000.0;
        let seg_end = item["offsets"]["to"].as_f64().unwrap_or(0.0) / 1000.0;
        let words = words_from_tokens(item["tokens"].as_array().map(Vec::as_slice).unwrap_or(&[]), seg_start, seg_end);
        if words.is_empty() {
            continue;
        }
        let text = item["text"].as_str().unwrap_or("").trim().to_string();
        if is_annotation(&text) {
            continue; // "[Musica]", "(aplausos)": no es habla
        }
        segments.push(Segment { start: seg_start, end: seg_end.max(words.last().map_or(seg_end, |w| w.end)), text, words });
    }
    Ok(EngineOutput { language, segments })
}

/// Quita los bucles en los que whisper se queda atascado: cuando se pierde,
/// repite trozos de lo ultimo que oyo ("...son bichos que..." cada 2 s) durante
/// un buen rato aunque ahi se hable de otra cosa o no se hable. Un segmento es
/// un eco si todas sus palabras estan en los dos anteriores; una racha de dos o
/// mas ecos seguidos es un bucle y se quita entera. Un eco suelto se deja: la
/// gente repite ("no... no").
pub fn drop_repetition_loops(segments: Vec<Segment>) -> Vec<Segment> {
    fn fold(text: &str) -> Vec<String> {
        text.split_whitespace()
            .map(|w| w.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect::<String>())
            .filter(|w| !w.is_empty())
            .collect()
    }
    fn same(a: &str, b: &str) -> bool {
        a == b || (a.chars().count().min(b.chars().count()) >= 4 && (a.contains(b) || b.contains(a)))
    }
    let folded: Vec<Vec<String>> = segments.iter().map(|s| fold(&s.text)).collect();
    let echo: Vec<bool> = (0..segments.len())
        .map(|i| {
            let words = &folded[i];
            let recent: Vec<&String> = folded[i.saturating_sub(2)..i].iter().flatten().collect();
            !words.is_empty() && words.len() <= 8 && words.iter().all(|w| recent.iter().any(|r| same(w, r)))
        })
        .collect();

    let mut keep = vec![true; segments.len()];
    let mut i = 0;
    while i < segments.len() {
        let run = echo[i..].iter().take_while(|&&e| e).count();
        if run >= 2 {
            log::info!(
                "Quitado un bucle de la transcripcion ({} trozos repetidos desde {:.0} s: \"{}\").",
                run,
                segments[i].start,
                segments[i].text.trim()
            );
            keep[i..i + run].iter_mut().for_each(|k| *k = false);
        }
        i += run.max(1);
    }
    segments.into_iter().zip(keep).filter_map(|(s, k)| k.then_some(s)).collect()
}

/// Anotaciones de sonido que whisper escribe en vez de habla.
fn is_annotation(text: &str) -> bool {
    let t = text.trim();
    t.is_empty()
        || (t.starts_with('[') && t.ends_with(']'))
        || (t.starts_with('(') && t.ends_with(')'))
        || t.starts_with('♪')
}

/// Une los tokens en palabras: un token que empieza por espacio abre palabra
/// nueva; el resto (sufijos, puntuacion) se pega a la anterior.
fn words_from_tokens(tokens: &[Value], seg_start: f64, seg_end: f64) -> Vec<Word> {
    struct Building {
        text: String,
        start: f64,
        end: f64,
        probs: Vec<f64>,
    }
    let mut words: Vec<Building> = Vec::new();

    for token in tokens {
        let text = token["text"].as_str().unwrap_or("");
        if text.is_empty() || (text.starts_with("[_") && text.ends_with(']')) || (text.starts_with("<|") && text.ends_with("|>")) {
            continue; // tokens especiales (inicio, marcas de tiempo, fin)
        }
        let start = token["offsets"]["from"].as_f64().map_or(seg_start, |ms| ms / 1000.0);
        let end = token["offsets"]["to"].as_f64().map_or(seg_end, |ms| ms / 1000.0).max(start);
        let probability = token["p"].as_f64().unwrap_or(1.0);

        match words.last_mut() {
            Some(word) if !text.starts_with(' ') => {
                word.text.push_str(text);
                word.end = word.end.max(end);
                word.probs.push(probability);
            }
            _ => words.push(Building { text: text.to_string(), start, end, probs: vec![probability] }),
        }
    }

    words
        .into_iter()
        .filter(|w| !w.text.trim().is_empty())
        .map(|w| Word {
            text: w.text.trim().to_string(),
            start: w.start,
            end: w.end,
            probability: w.probs.iter().sum::<f64>() / w.probs.len() as f64,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    // -- reintento en CPU -------------------------------------------------------

    #[derive(Default)]
    struct Estado {
        usos: Vec<Device>,
    }

    struct Falso {
        estado: Arc<Mutex<Estado>>,
        gpu_falla_con: Option<&'static str>,
        error_en_cpu: Option<&'static str>,
        tiene_gpu: bool,
    }

    impl Falso {
        fn nuevo(estado: &Arc<Mutex<Estado>>) -> Self {
            Self { estado: estado.clone(), gpu_falla_con: Some("Library cublas64_12.dll is not found or cannot be loaded"), error_en_cpu: None, tiene_gpu: true }
        }
    }

    impl Engine for Falso {
        fn cuda_available(&self) -> bool {
            self.tiene_gpu
        }

        fn transcribe(&mut self, _wav: &Path, device: Device, _o: &EngineOptions, on_progress: &mut dyn FnMut(f64)) -> Result<EngineOutput> {
            self.estado.lock().unwrap().usos.push(device);
            if device == Device::Cuda {
                if let Some(motivo) = self.gpu_falla_con {
                    return Err(TranscribeError(motivo.into()));
                }
            } else if let Some(motivo) = self.error_en_cpu {
                return Err(TranscribeError(motivo.into()));
            }
            on_progress(0.5);
            Ok(EngineOutput {
                language: Some("es".into()),
                segments: vec![Segment { start: 0.0, end: 2.0, text: "hola que tal".into(), words: vec![Word::new("hola", 0.0, 1.0), Word::new("que", 1.0, 1.5), Word::new("tal.", 1.5, 2.0)] }],
            })
        }
    }

    fn transcriptor(motor: Falso, dispositivo: &str) -> (Transcriber, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let cfg = TranscribeConfig { device: dispositivo.into(), ..Default::default() };
        (Transcriber::new(cfg, dir.path().join("cache"), Box::new(motor)).unwrap(), dir)
    }

    fn pal(texto: &str, a: f64) -> Word {
        Word::new(texto, a, a + 0.4)
    }

    #[test]
    fn las_correcciones_cambian_palabras_y_conservan_la_puntuacion() {
        let c = Corrections::parse("spotifai=Spotify; la guapa=La Guapa ;roto;=nada;x=");
        assert!(!c.is_empty());
        let fuera = c.apply_words(vec![pal("Oye,", 0.0), pal("Spotifai.", 0.5), pal("y", 1.0), pal("la", 1.5), pal("guapa", 2.0), pal("de", 2.5), pal("redes", 3.0)]);
        let textos: Vec<&str> = fuera.iter().map(|w| w.text.as_str()).collect();
        assert_eq!(textos, ["Oye,", "Spotify.", "y", "La Guapa", "de", "redes"]);
        let guapa = &fuera[3];
        assert!((guapa.start - 1.5).abs() < 1e-9 && (guapa.end - 2.4).abs() < 1e-9, "una sola palabra que cubre las dos");
        assert!(Corrections::parse("").is_empty() && Corrections::parse("sin igual").is_empty());
    }

    #[test]
    fn el_vocabulario_se_convierte_en_una_frase_para_whisper() {
        assert_eq!(vocabulary_prompt("   "), None);
        assert_eq!(vocabulary_prompt("Ibai, Auronplay, Twitch."), Some("Nombres y términos de la conversación: Ibai, Auronplay, Twitch.".into()));
        assert!(!vocabulary_prompt("a\nb").unwrap().contains('\n'));
    }

    #[test]
    fn las_frases_en_directo_se_leen_con_su_tiempo() {
        assert_eq!(parse_live_line("[00:01:02.500 --> 00:01:05.000]   Hola, esto es una prueba."), Some((62.5, "Hola, esto es una prueba.".to_string())));
        assert_eq!(parse_live_line("[01:00:00.000 --> 01:00:02.000] Una hora").map(|l| l.0), Some(3600.0));
        for no in ["", "whisper_init: loading model", "[00:00:01.000 --> 00:00:02.000]   [Música]", "[00:00:01.000 --> 00:00:02.000]   ", "[sin tiempos] hola"] {
            assert_eq!(parse_live_line(no), None, "{no}");
        }
    }

    #[test]
    fn cae_a_cpu_cuando_cuda_falla_a_mitad() {
        let estado = Arc::new(Mutex::new(Estado::default()));
        let (mut t, _dir) = transcriptor(Falso::nuevo(&estado), "auto");
        let salida = t.run(Path::new("a.wav"), &mut |_| {}).unwrap();
        assert_eq!(estado.lock().unwrap().usos, [Device::Cuda, Device::Cpu]);
        assert_eq!(salida.segments.len(), 1, "no se pierde ningun segmento");
    }

    #[test]
    fn no_reintenta_la_gpu_una_vez_ha_fallado() {
        let estado = Arc::new(Mutex::new(Estado::default()));
        let (mut t, _dir) = transcriptor(Falso::nuevo(&estado), "auto");
        t.run(Path::new("a.wav"), &mut |_| {}).unwrap();
        t.run(Path::new("b.wav"), &mut |_| {}).unwrap();
        // La segunda llamada no vuelve a probar la GPU: seria un minuto tirado
        // por cada zona del podcast.
        assert_eq!(estado.lock().unwrap().usos, [Device::Cuda, Device::Cpu, Device::Cpu]);
    }

    #[test]
    fn la_gpu_que_funciona_se_usa() {
        let estado = Arc::new(Mutex::new(Estado::default()));
        let motor = Falso { gpu_falla_con: None, ..Falso::nuevo(&estado) };
        let (mut t, _dir) = transcriptor(motor, "auto");
        t.run(Path::new("a.wav"), &mut |_| {}).unwrap();
        assert_eq!(estado.lock().unwrap().usos, [Device::Cuda]);
    }

    #[test]
    fn un_error_que_no_es_de_gpu_no_se_traga() {
        let estado = Arc::new(Mutex::new(Estado::default()));
        let motor = Falso { gpu_falla_con: Some("el audio esta corrupto"), ..Falso::nuevo(&estado) };
        let (mut t, _dir) = transcriptor(motor, "auto");
        // Reintentar en CPU no arreglaria esto: tiene que salir a la superficie.
        assert!(t.run(Path::new("a.wav"), &mut |_| {}).unwrap_err().0.contains("corrupto"));
        assert_eq!(estado.lock().unwrap().usos, [Device::Cuda]);
    }

    #[test]
    fn si_tambien_falla_en_cpu_el_error_llega() {
        let estado = Arc::new(Mutex::new(Estado::default()));
        let motor = Falso { error_en_cpu: Some("sin memoria"), ..Falso::nuevo(&estado) };
        let (mut t, _dir) = transcriptor(motor, "auto");
        assert!(t.run(Path::new("a.wav"), &mut |_| {}).is_err());
    }

    #[test]
    fn sin_gpu_o_con_cpu_forzada_no_se_toca_cuda() {
        for (dispositivo, tiene_gpu) in [("auto", false), ("cpu", true)] {
            let estado = Arc::new(Mutex::new(Estado::default()));
            let motor = Falso { tiene_gpu, ..Falso::nuevo(&estado) };
            let (mut t, _dir) = transcriptor(motor, dispositivo);
            t.run(Path::new("a.wav"), &mut |_| {}).unwrap();
            assert_eq!(estado.lock().unwrap().usos, [Device::Cpu], "{dispositivo}");
        }
    }

    #[test]
    fn el_progreso_del_motor_llega_a_quien_lo_pide() {
        let estado = Arc::new(Mutex::new(Estado::default()));
        let (mut t, _dir) = transcriptor(Falso { gpu_falla_con: None, ..Falso::nuevo(&estado) }, "cpu");
        let mut visto = Vec::new();
        t.run(Path::new("a.wav"), &mut |f| visto.push(f)).unwrap();
        assert_eq!(visto, [0.5]);
    }

    #[test]
    fn reconocer_el_fallo_de_gpu() {
        for texto in [
            "Library cublas64_12.dll is not found or cannot be loaded",
            "Unable to load libcudnn_ops.so.9",
            "CUDA failed with error out of memory",
            "no kernel image is available for execution on the device",
        ] {
            assert!(is_gpu_failure(texto), "{texto}");
        }
        for texto in ["el fichero no existe", "audio vacio", "timeout"] {
            assert!(!is_gpu_failure(texto), "{texto}");
        }
    }

    // -- JSON de whisper.cpp ------------------------------------------------------

    fn token(texto: &str, desde: i64, hasta: i64, p: f64) -> Value {
        json!({"text": texto, "offsets": {"from": desde, "to": hasta}, "p": p})
    }

    fn muestra() -> String {
        json!({
            "result": {"language": "es"},
            "transcription": [
                {"offsets": {"from": 0, "to": 3000}, "text": " Hola, esto es una prueba.", "tokens": [
                    token("[_BEG_]", 0, 0, 1.0),
                    token(" Hola", 100, 600, 0.9),
                    token(",", 600, 650, 0.8),
                    token(" esto", 700, 900, 0.9),
                    token(" es", 900, 1000, 0.9),
                    token(" una", 1000, 1200, 0.9),
                    token(" pru", 1300, 1500, 0.6),
                    token("eba", 1500, 1800, 0.8),
                    token(".", 1800, 1900, 0.7),
                    token("[_TT_150]", 3000, 3000, 1.0),
                ]},
                {"offsets": {"from": 3000, "to": 5000}, "text": " [Música]", "tokens": [token(" [", 3000, 3200, 0.5), token("Música", 3200, 4800, 0.5), token("]", 4800, 5000, 0.5)]},
                {"offsets": {"from": 5000, "to": 6000}, "text": "", "tokens": []},
            ]
        })
        .to_string()
    }

    #[test]
    fn las_palabras_se_arman_con_sus_tokens() {
        let salida = parse_whisper_json(&muestra()).unwrap();
        assert_eq!(salida.language.as_deref(), Some("es"));
        assert_eq!(salida.segments.len(), 1, "la anotacion de musica y el segmento vacio se descartan");
        let palabras: Vec<&str> = salida.segments[0].words.iter().map(|w| w.text.as_str()).collect();
        assert_eq!(palabras, ["Hola,", "esto", "es", "una", "prueba."]);
        let prueba = &salida.segments[0].words[4];
        assert_eq!((prueba.start, prueba.end), (1.3, 1.9));
        assert!((prueba.probability - (0.6 + 0.8 + 0.7) / 3.0).abs() < 1e-9);
        assert!(salida.segments[0].words[4].ends_sentence());
    }

    #[test]
    fn los_tiempos_de_las_palabras_nunca_van_hacia_atras() {
        let raro = json!({"transcription": [{"offsets": {"from": 0, "to": 2000}, "text": " a b", "tokens": [token(" a", 500, 100, 1.0), token(" b", 900, 1000, 1.0)]}]}).to_string();
        let palabras = &parse_whisper_json(&raro).unwrap().segments[0].words;
        assert!(palabras.iter().all(|w| w.end >= w.start));
    }

    #[test]
    fn resultado_ilegible_o_sin_habla() {
        assert!(parse_whisper_json("no es json").is_err());
        assert!(parse_whisper_json("{}").unwrap().segments.is_empty());
    }

    #[test]
    fn el_progreso_que_imprime_whisper() {
        assert_eq!(parse_progress("whisper_print_progress_callback: progress =  45%"), Some(0.45));
        assert_eq!(parse_progress("whisper_print_progress_callback: progress = 100%"), Some(1.0));
        assert_eq!(parse_progress("whisper_init_state: kv self size = 100 MB"), None);
    }

    // -- cache --------------------------------------------------------------------

    #[test]
    fn la_clave_de_cache_cambia_con_lo_que_cambia_el_resultado() {
        let dir = tempfile::tempdir().unwrap();
        let video = dir.path().join("v.mp4");
        std::fs::write(&video, b"x").unwrap();
        let estado = Arc::new(Mutex::new(Estado::default()));
        let (t, _d) = transcriptor(Falso::nuevo(&estado), "auto");

        let completa = t.cache_key(&video, None, true).unwrap();
        assert_eq!(completa, t.cache_key(&video, None, true).unwrap());
        assert_ne!(completa, t.cache_key(&video, Some(&[(0.0, 30.0)]), true).unwrap(), "zonas distintas no comparten entrada");
        assert_ne!(t.cache_key(&video, Some(&[(0.0, 30.0)]), true).unwrap(), t.cache_key(&video, Some(&[(0.0, 31.0)]), true).unwrap());

        let (otro, _d2) = {
            let dir2 = tempfile::tempdir().unwrap();
            let cfg = TranscribeConfig { model_size: "tiny".into(), ..Default::default() };
            (Transcriber::new(cfg, dir2.path().join("c"), Box::new(Falso::nuevo(&estado))).unwrap(), dir2)
        };
        assert_ne!(completa, otro.cache_key(&video, None, true).unwrap(), "otro modelo, otra transcripcion");
        assert_eq!(completa.len(), 16);
    }

    #[test]
    fn la_transcripcion_sobrevive_a_borrar_y_volver_a_bajar_el_video() {
        let dir = tempfile::tempdir().unwrap();
        let video = dir.path().join("podcast.mp4");
        std::fs::write(&video, b"descarga uno").unwrap();
        let estado = Arc::new(Mutex::new(Estado::default()));
        let (t, _d) = transcriptor(Falso::nuevo(&estado), "auto");
        let guardada = Transcript { language: "es".into(), duration: 9.0, segments: vec![seg(0.0, "Hola.")] };

        // Una cache de antes (por fichero) se encuentra y pasa a guardarse por enlace.
        write_cache(&t.cache_dir.join(format!("{}.json", t.cache_key(&video, None, false).unwrap())), &guardada);
        std::fs::write(dir.path().join("podcast.source"), "yt:dQw4w9WgXcQ").unwrap();
        assert_eq!(t.cached(&video, None).unwrap().1.as_ref(), Some(&guardada));

        // Se borra y se vuelve a bajar: otra fecha y otro tamano, mismo enlace.
        std::fs::remove_file(&video).unwrap();
        std::fs::write(&video, b"descarga dos, algo distinta").unwrap();
        assert_eq!(t.cached(&video, None).unwrap().1.as_ref(), Some(&guardada), "no se vuelve a transcribir");

        // Otro enlace en el mismo sitio no la aprovecha.
        std::fs::write(dir.path().join("podcast.source"), "yt:AAAAAAAAAAA").unwrap();
        assert!(t.cached(&video, None).unwrap().1.is_none());
        // Y un fichero propio (sin marca) sigue identificandose por el fichero.
        std::fs::remove_file(dir.path().join("podcast.source")).unwrap();
        assert!(t.cached(&video, None).unwrap().1.is_none());
    }

    #[test]
    fn la_cache_ilegible_se_ignora() {
        let dir = tempfile::tempdir().unwrap();
        let roto = dir.path().join("x.json");
        std::fs::write(&roto, "{roto").unwrap();
        assert!(read_cache(&roto).is_none());
        assert!(read_cache(&dir.path().join("no_existe.json")).is_none());
    }

    #[test]
    fn desplazar_un_segmento_mueve_tambien_sus_palabras() {
        let s = Segment { start: 1.0, end: 2.0, text: "a".into(), words: vec![Word::new("a", 1.0, 2.0)] };
        let movido = shift(s, 100.0);
        assert_eq!((movido.start, movido.end, movido.words[0].start), (101.0, 102.0, 101.0));
    }

    fn seg(a: f64, texto: &str) -> Segment {
        Segment { start: a, end: a + 2.0, text: texto.into(), words: vec![Word::new(texto, a, a + 2.0)] }
    }

    #[test]
    fn el_bucle_de_whisper_se_quita_entero() {
        // Copiado de una transcripcion real: whisper se atasca y repite.
        let entrada = vec![
            seg(330.0, "Eso funciona."),
            seg(334.74, "Yo creo que los que me han spawneados son bichos que..."),
            seg(338.74, "...spawneados..."),
            seg(340.74, "...son bichos que..."),
            seg(342.74, "...espawneados..."),
            seg(344.74, "...son bichos que..."),
            seg(346.74, "...son bichos que..."),
            seg(352.0, "Vale, ahora a por el cofre."),
        ];
        let textos: Vec<String> = drop_repetition_loops(entrada).into_iter().map(|s| s.text).collect();
        assert_eq!(textos, ["Eso funciona.", "Yo creo que los que me han spawneados son bichos que...", "Vale, ahora a por el cofre."]);
    }

    #[test]
    fn repetir_una_vez_es_hablar_y_se_queda() {
        let entrada = vec![seg(0.0, "¿Qué ha pasado?"), seg(2.0, "No, no."), seg(4.0, "No."), seg(6.0, "Mira esto."), seg(8.0, "Mira.")];
        assert_eq!(drop_repetition_loops(entrada.clone()), entrada);
    }

    #[test]
    fn formato_de_duracion() {
        assert_eq!(fmt_duration(75.0), "1m15s");
        assert_eq!(fmt_duration(3725.0), "1h02m05s");
    }
}
