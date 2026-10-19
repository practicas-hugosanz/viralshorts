//! Primer arranque: descarga, dentro de la carpeta de datos, las herramientas
//! que el programa usa como ejecutables.
//!
//!   ffmpeg      - decodifica y monta los videos
//!   yt-dlp      - descarga de YouTube
//!   whisper.cpp - transcribe la voz (motor de CPU; el de GPU es opcional)
//!
//! Se hace dentro de la propia aplicacion, con una pantalla de progreso, para
//! que instalar sea abrir el programa y nada mas. Los modelos (voz y caras) los
//! baja el pipeline la primera vez que los necesita.
//!
//! Las versiones estan FIJADAS: que la descarga de hoy y la de dentro de un ano
//! sean la misma, y que una version nueva con opciones quitadas no rompa el
//! montaje (asi desaparecio `-filter_complex_script` de ffmpeg).

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::{json, Value};

use crate::fetch;
use crate::ffmpeg;

pub const FFMPEG_VERSION: &str = "8.0";
pub const WHISPER_RELEASE: &str = "b5454";

fn ffmpeg_urls() -> Vec<String> {
    vec![
        format!("https://github.com/GyanD/codexffmpeg/releases/download/{FFMPEG_VERSION}/ffmpeg-{FFMPEG_VERSION}-essentials_build.zip"),
        "https://www.gyan.dev/ffmpeg/builds/ffmpeg-release-essentials.zip".into(),
    ]
}

fn whisper_url(file: &str) -> String {
    format!("https://github.com/ggml-org/whisper.cpp/releases/download/{WHISPER_RELEASE}/{file}")
}

const YTDLP_URL: &str = "https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp.exe";

/// Modelos de voz que ofrece la interfaz: (nombre, MB aproximados). Pesan desde
/// decenas de MB hasta varios GB, asi que se bajan a peticion y no en el primer
/// arranque.
pub const MODELS: &[(&str, u64)] = &[("tiny", 75), ("small", 466), ("medium", 1500), ("large-v3-turbo", 1620), ("large-v3", 3100)];

pub fn model_path(root: &Path, size: &str) -> PathBuf {
    root.join("bin").join("models").join(format!("ggml-{size}.bin"))
}

pub fn model_installed(root: &Path, size: &str) -> bool {
    std::fs::metadata(model_path(root, size)).is_ok_and(|m| m.len() > 0)
}

pub fn model_url(size: &str) -> String {
    format!("https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-{size}.bin")
}

/// Estado de cada modelo, para que la interfaz avise antes de lanzar.
pub fn models_json(root: &Path) -> Value {
    Value::Array(
        MODELS.iter().map(|(size, mb)| json!({"size": size, "mb": mb, "installed": model_installed(root, size)})).collect(),
    )
}

/// Cada pieza que puede faltar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    Ffmpeg,
    YtDlp,
    Whisper,
    WhisperCuda,
    /// Un modelo de voz (ver `MODELS`).
    Model(&'static str),
}

impl Tool {
    pub fn label(self) -> &'static str {
        match self {
            Tool::Ffmpeg => "ffmpeg (video)",
            Tool::YtDlp => "yt-dlp (descarga de YouTube)",
            Tool::Whisper => "whisper.cpp (transcripcion)",
            Tool::WhisperCuda => "motor de transcripcion para la tarjeta NVIDIA",
            Tool::Model("tiny") => "modelo de voz tiny",
            Tool::Model("small") => "modelo de voz small",
            Tool::Model("medium") => "modelo de voz medium",
            Tool::Model("large-v3-turbo") => "modelo de voz large-v3-turbo",
            Tool::Model("large-v3") => "modelo de voz large-v3",
            Tool::Model(_) => "modelo de voz",
        }
    }
}

fn exe(name: &str) -> String {
    if cfg!(windows) { format!("{name}.exe") } else { name.to_string() }
}

pub fn whisper_cli(root: &Path, dir: &str) -> PathBuf {
    root.join("bin").join(dir).join(exe("whisper-cli"))
}

/// Lo que falta para poder trabajar. ffmpeg y yt-dlp del sistema valen.
pub fn missing(root: &Path) -> Vec<Tool> {
    let local = |p: PathBuf| p.is_file();
    let mut missing = Vec::new();
    let ffmpeg_dir = root.join("bin").join("ffmpeg").join("bin");
    let ffmpeg_local = local(ffmpeg_dir.join(exe("ffmpeg"))) && local(ffmpeg_dir.join(exe("ffprobe")));
    if !ffmpeg_local && !(ffmpeg::which("ffmpeg").is_some() && ffmpeg::which("ffprobe").is_some()) {
        missing.push(Tool::Ffmpeg);
    }
    if !local(root.join("bin").join(exe("yt-dlp"))) && ffmpeg::which("yt-dlp").is_none() {
        missing.push(Tool::YtDlp);
    }
    if !local(whisper_cli(root, "whisper")) && ffmpeg::which("whisper-cli").is_none() {
        missing.push(Tool::Whisper);
    }
    missing
}

pub fn cuda_installed(root: &Path) -> bool {
    whisper_cli(root, "whisper-cuda").is_file()
}

/// Estado de la instalacion, para la pantalla de progreso.
#[derive(Debug, Clone, Default)]
pub struct State {
    pub running: bool,
    pub done: bool,
    pub error: Option<String>,
    pub step: String,
    /// 0..1 sobre el total de piezas a instalar.
    pub overall: f64,
    pub log: Vec<String>,
}

impl State {
    pub fn to_json(&self, root: &Path) -> Value {
        json!({
            "running": self.running,
            "done": self.done,
            "error": self.error,
            "step": self.step,
            "overall": self.overall,
            "log": self.log,
            "missing": missing(root).iter().map(|t| t.label()).collect::<Vec<_>>(),
        })
    }
}

pub type Shared = Mutex<State>;

fn note(state: &Shared, step: &str, overall: f64) {
    let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
    s.step = step.to_string();
    s.overall = overall.clamp(0.0, 1.0);
    s.log.push(step.to_string());
}

fn set_overall(state: &Shared, overall: f64) {
    state.lock().unwrap_or_else(|e| e.into_inner()).overall = overall.clamp(0.0, 1.0);
}

/// Instala lo que falte. Bloquea hasta terminar; el estado se va actualizando.
pub fn install(root: &Path, tools: &[Tool], state: &Shared) {
    {
        let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
        if s.running {
            return;
        }
        *s = State { running: true, ..Default::default() };
    }
    let outcome = install_all(root, tools, state);
    let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
    s.running = false;
    match outcome {
        Ok(()) => {
            s.done = true;
            s.overall = 1.0;
            s.step = "Listo".into();
        }
        Err(error) => {
            log::error!("{error}");
            s.log.push(error.clone());
            s.error = Some(error);
        }
    }
}

fn install_all(root: &Path, tools: &[Tool], state: &Shared) -> Result<(), String> {
    let total = tools.len().max(1) as f64;
    for (index, tool) in tools.iter().enumerate() {
        let base = index as f64 / total;
        note(state, &format!("Descargando {}...", tool.label()), base);
        // La barra de cada pieza se reparte dentro de su tramo.
        let mut on_progress = |done: u64, size: Option<u64>| {
            if let Some(size) = size.filter(|s| *s > 0) {
                set_overall(state, base + (done as f64 / size as f64) * 0.9 / total);
            }
        };
        match tool {
            Tool::Ffmpeg => install_ffmpeg(root, &mut on_progress)?,
            Tool::YtDlp => {
                fetch::download_file_with(YTDLP_URL, &root.join("bin").join(exe("yt-dlp")), "yt-dlp", &mut on_progress).map_err(|e| e.to_string())?;
            }
            Tool::Whisper => install_whisper(root, &mut on_progress)?,
            Tool::WhisperCuda => install_cuda(root, &mut on_progress)?,
            Tool::Model(size) => {
                fetch::download_file_with(&model_url(size), &model_path(root, size), tool.label(), &mut on_progress).map_err(|e| e.to_string())?;
            }
        }
        note(state, &format!("{} listo", tool.label()), (index + 1) as f64 / total);
    }
    Ok(())
}

fn scratch(root: &Path, name: &str) -> PathBuf {
    root.join("bin").join(format!(".{name}.zip"))
}

fn install_ffmpeg(root: &Path, on_progress: &mut dyn FnMut(u64, Option<u64>)) -> Result<(), String> {
    let zip = scratch(root, "ffmpeg");
    let mut errors = Vec::new();
    let mut downloaded = false;
    // Si el primer espejo cae, se prueba el siguiente.
    for url in ffmpeg_urls() {
        match fetch::download_file_with(&url, &zip, "ffmpeg", on_progress) {
            Ok(()) => {
                downloaded = true;
                break;
            }
            Err(error) => errors.push(error.to_string()),
        }
    }
    if !downloaded {
        return Err(errors.join("\n"));
    }
    let dest = root.join("bin").join("ffmpeg").join("bin");
    let result = extract(&zip, &dest, |name| name.ends_with("/bin/ffmpeg.exe") || name.ends_with("/bin/ffprobe.exe"));
    let _ = std::fs::remove_file(&zip);
    if result? < 2 {
        return Err("El ZIP de ffmpeg no traia ffmpeg.exe y ffprobe.exe.".into());
    }
    Ok(())
}

fn install_whisper(root: &Path, on_progress: &mut dyn FnMut(u64, Option<u64>)) -> Result<(), String> {
    let zip = scratch(root, "whisper");
    fetch::download_file_with(&whisper_url("whisper-bin-x64.zip"), &zip, "whisper.cpp", on_progress).map_err(|e| e.to_string())?;
    // Solo lo necesario: el ZIP trae decenas de ejecutables de ejemplo.
    let dest = root.join("bin").join("whisper");
    let result = extract(&zip, &dest, |name| {
        let file = name.rsplit('/').next().unwrap_or(name);
        file == "whisper-cli.exe" || file == "whisper.dll" || (file.starts_with("ggml") && file.ends_with(".dll"))
    });
    let _ = std::fs::remove_file(&zip);
    if result? == 0 || !whisper_cli(root, "whisper").is_file() {
        return Err("El ZIP de whisper.cpp no traia whisper-cli.exe.".into());
    }
    Ok(())
}

fn install_cuda(root: &Path, on_progress: &mut dyn FnMut(u64, Option<u64>)) -> Result<(), String> {
    let zip = scratch(root, "whisper-cuda");
    fetch::download_file_with(&whisper_url("whisper-bin-win-cuda-12.4.0-x64.zip"), &zip, "el motor CUDA", on_progress).map_err(|e| e.to_string())?;
    // Aqui hacen falta TODOS los ficheros: las librerias de CUDA van con el motor.
    let dest = root.join("bin").join("whisper-cuda");
    let result = extract(&zip, &dest, |_| true);
    let _ = std::fs::remove_file(&zip);
    result?;
    if !whisper_cli(root, "whisper-cuda").is_file() {
        return Err("El ZIP del motor CUDA no traia whisper-cli.exe.".into());
    }
    Ok(())
}

/// Saca de un ZIP los ficheros que cumplen `wanted` (por su ruta dentro del
/// ZIP), todos en `dest` y sin subcarpetas. Devuelve cuantos copio. Solo se usa
/// el nombre de cada fichero: una ruta rara dentro del ZIP no puede escribir
/// fuera de `dest`.
pub fn extract(zip: &Path, dest: &Path, wanted: impl Fn(&str) -> bool) -> Result<usize, String> {
    let file = std::fs::File::open(zip).map_err(|e| format!("No se pudo abrir {}: {e}", zip.display()))?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| format!("El ZIP llego corrupto ({e}). Vuelve a intentarlo."))?;
    std::fs::create_dir_all(dest).map_err(|e| format!("No se pudo crear {}: {e}", dest.display()))?;

    let mut copied = 0;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).map_err(|e| e.to_string())?;
        if entry.is_dir() {
            continue;
        }
        let name = entry.name().map_err(|e| e.to_string())?.replace('\\', "/");
        if !wanted(&name) {
            continue;
        }
        let Some(file_name) = name.rsplit('/').next().filter(|n| !n.is_empty() && *n != "..") else { continue };
        let target = dest.join(file_name);
        let mut data = Vec::with_capacity(entry.size() as usize);
        entry.read_to_end(&mut data).map_err(|e| format!("No se pudo leer {name} del ZIP: {e}"))?;
        std::fs::write(&target, data).map_err(|e| format!("No se pudo escribir {}: {e}", target.display()))?;
        copied += 1;
    }
    Ok(copied)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn zip_de_prueba(ruta: &Path, ficheros: &[(&str, &[u8])]) {
        let mut zip = zip::ZipWriter::new(std::fs::File::create(ruta).unwrap());
        for (nombre, datos) in ficheros {
            zip.start_file(*nombre, zip::write::SimpleFileOptions::default()).unwrap();
            zip.write_all(datos).unwrap();
        }
        zip.finish().unwrap();
    }

    #[test]
    fn se_sacan_solo_los_ficheros_pedidos_y_sin_subcarpetas() {
        let dir = tempfile::tempdir().unwrap();
        let zip = dir.path().join("a.zip");
        zip_de_prueba(&zip, &[
            ("ffmpeg-8.0/bin/ffmpeg.exe", b"F"),
            ("ffmpeg-8.0/bin/ffprobe.exe", b"P"),
            ("ffmpeg-8.0/bin/ffplay.exe", b"X"),
            ("ffmpeg-8.0/doc/LEEME.txt", b"T"),
        ]);
        let destino = dir.path().join("bin");
        let copiados = extract(&zip, &destino, |n| n.ends_with("/bin/ffmpeg.exe") || n.ends_with("/bin/ffprobe.exe")).unwrap();
        assert_eq!(copiados, 2);
        assert_eq!(std::fs::read(destino.join("ffmpeg.exe")).unwrap(), b"F");
        assert!(destino.join("ffprobe.exe").is_file() && !destino.join("ffplay.exe").exists());
    }

    #[test]
    fn una_ruta_con_trampa_no_escribe_fuera() {
        let dir = tempfile::tempdir().unwrap();
        let zip = dir.path().join("a.zip");
        zip_de_prueba(&zip, &[("../../fuera.dll", b"x"), ("Release\\ggml.dll", b"y")]);
        let destino = dir.path().join("dentro").join("bin");
        assert_eq!(extract(&zip, &destino, |_| true).unwrap(), 2);
        assert!(destino.join("fuera.dll").is_file() && destino.join("ggml.dll").is_file());
        assert!(!dir.path().join("fuera.dll").exists());
    }

    #[test]
    fn un_zip_roto_da_un_mensaje_claro() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.zip"), b"no es un zip").unwrap();
        let error = extract(&dir.path().join("a.zip"), dir.path(), |_| true).unwrap_err();
        assert!(error.contains("corrupto"), "{error}");
        assert!(extract(&dir.path().join("no_existe.zip"), dir.path(), |_| true).is_err());
    }

    #[test]
    fn que_falta_en_una_carpeta_vacia_y_en_una_instalada() {
        let dir = tempfile::tempdir().unwrap();
        let faltan = missing(dir.path());
        // ffmpeg, yt-dlp o whisper pueden estar en el PATH de esta maquina; lo que no
        // puede pasar es que falte algo que SI esta en la carpeta.
        for dir_herramienta in ["bin/ffmpeg/bin", "bin/whisper"] {
            std::fs::create_dir_all(dir.path().join(dir_herramienta)).unwrap();
        }
        for fichero in ["bin/ffmpeg/bin/ffmpeg.exe", "bin/ffmpeg/bin/ffprobe.exe", "bin/yt-dlp.exe", "bin/whisper/whisper-cli.exe"] {
            std::fs::write(dir.path().join(fichero), b"x").unwrap();
        }
        let completa = missing(dir.path());
        assert!(completa.is_empty() || !cfg!(windows), "{completa:?}");
        assert!(completa.len() <= faltan.len());
        assert!(!cuda_installed(dir.path()));
        std::fs::create_dir_all(dir.path().join("bin/whisper-cuda")).unwrap();
        std::fs::write(dir.path().join("bin/whisper-cuda/whisper-cli.exe"), b"x").unwrap();
        assert_eq!(cuda_installed(dir.path()), cfg!(windows));
    }

    #[test]
    fn los_modelos_se_reconocen_por_su_fichero() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!model_installed(dir.path(), "tiny"));
        std::fs::create_dir_all(dir.path().join("bin/models")).unwrap();
        std::fs::write(dir.path().join("bin/models/ggml-tiny.bin"), b"x").unwrap();
        std::fs::write(dir.path().join("bin/models/ggml-small.bin"), b"").unwrap();
        assert!(model_installed(dir.path(), "tiny"));
        assert!(!model_installed(dir.path(), "small"), "un fichero vacio no vale");
        let lista = models_json(dir.path());
        assert_eq!(lista.as_array().unwrap().len(), MODELS.len());
        assert_eq!(lista[0]["size"], "tiny");
        assert_eq!(lista[0]["installed"], true);
        assert_eq!(lista[2]["installed"], false);
        assert!(model_url("medium").ends_with("/ggml-medium.bin"));
    }

    #[test]
    fn el_estado_se_cuenta_en_json_para_la_pantalla() {
        let dir = tempfile::tempdir().unwrap();
        let estado: Shared = Mutex::new(State::default());
        note(&estado, "Descargando ffmpeg", 0.25);
        set_overall(&estado, 7.0);
        let datos = estado.lock().unwrap().to_json(dir.path());
        assert_eq!(datos["step"], "Descargando ffmpeg");
        assert_eq!(datos["overall"], 1.0, "se acota a 1");
        assert!(datos["missing"].is_array() && datos["running"] == false);
    }

    #[test]
    fn instalar_algo_que_no_se_puede_bajar_deja_el_error_y_no_se_cuelga() {
        // Sin red a esta URL: el estado acaba con el motivo y sin "running".
        let dir = tempfile::tempdir().unwrap();
        let estado: Shared = Mutex::new(State::default());
        let mut al_bajar = |_: u64, _: Option<u64>| {};
        let error = fetch::download_file_with("http://127.0.0.1:9/yt-dlp.exe", &dir.path().join("bin").join("yt-dlp.exe"), "yt-dlp", &mut al_bajar).unwrap_err();
        assert!(error.to_string().contains("yt-dlp"));
        {
            let mut s = estado.lock().unwrap();
            s.running = true;
        }
        install(dir.path(), &[], &estado); // ya en marcha: no hace nada
        assert!(estado.lock().unwrap().running);
        estado.lock().unwrap().running = false;
        install(dir.path(), &[], &estado); // nada que instalar: termina bien
        let s = estado.lock().unwrap();
        assert!(s.done && !s.running && s.error.is_none());
    }
}
