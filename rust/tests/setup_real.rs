//! El primer arranque de verdad: descarga ffmpeg, yt-dlp y whisper.cpp en una
//! carpeta vacia y comprueba que los ejecutables arrancan. Y que las direcciones
//! de todo lo demas (motor CUDA, modelos de voz) siguen existiendo.
//!
//! Gasta unos 200 MB de red, asi que solo corre con `VS_NET_TESTS=1`.

use std::path::Path;
use std::process::Command;
use std::sync::Mutex;

use viralshorts::setup::{self, State, Tool};

fn con_red() -> bool {
    std::env::var("VS_NET_TESTS").is_ok()
}

fn arranca(exe: &Path, args: &[&str]) -> String {
    let salida = Command::new(exe).args(args).output().unwrap_or_else(|e| panic!("no se pudo lanzar {}: {e}", exe.display()));
    assert!(salida.status.success(), "{} {:?} fallo: {}", exe.display(), args, String::from_utf8_lossy(&salida.stderr));
    format!("{}{}", String::from_utf8_lossy(&salida.stdout), String::from_utf8_lossy(&salida.stderr))
}

#[test]
fn las_herramientas_se_descargan_y_arrancan() {
    if !con_red() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let estado: setup::Shared = Mutex::new(State::default());
    setup::install(dir.path(), &[Tool::Ffmpeg, Tool::YtDlp, Tool::Whisper], &estado);
    {
        let s = estado.lock().unwrap();
        assert!(s.done && s.error.is_none(), "la instalacion fallo: {:?}\n{}", s.error, s.log.join("\n"));
    }
    let bin = dir.path().join("bin");
    assert!(arranca(&bin.join("ffmpeg").join("bin").join("ffmpeg.exe"), &["-version"]).contains("ffmpeg version"));
    assert!(arranca(&bin.join("ffmpeg").join("bin").join("ffprobe.exe"), &["-version"]).contains("ffprobe version"));
    assert!(!arranca(&bin.join("yt-dlp.exe"), &["--version"]).trim().is_empty());
    assert!(arranca(&setup::whisper_cli(dir.path(), "whisper"), &["--help"]).to_lowercase().contains("usage"));
    // Ya no falta nada (salvo lo que el sistema ponga en el PATH).
    assert!(setup::missing(dir.path()).is_empty(), "{:?}", setup::missing(dir.path()));
}

fn existe(url: &str) -> Result<(), String> {
    let agente = ureq::AgentBuilder::new().timeout_connect(std::time::Duration::from_secs(20)).timeout_read(std::time::Duration::from_secs(30)).user_agent("viralshorts").build();
    match agente.head(url).call() {
        Ok(_) => Ok(()),
        Err(error) => Err(format!("{url}: {error}")),
    }
}

#[test]
fn las_direcciones_fijadas_siguen_existiendo() {
    if !con_red() {
        return;
    }
    let mut direcciones: Vec<String> = setup::ffmpeg_urls();
    direcciones.push(setup::YTDLP_URL.to_string());
    direcciones.push(setup::whisper_url("whisper-bin-x64.zip"));
    direcciones.push(setup::whisper_url("whisper-bin-win-cuda-12.4.0-x64.zip"));
    direcciones.extend(setup::MODELS.iter().map(|(modelo, _)| setup::model_url(modelo)));
    let fallos: Vec<String> = direcciones.iter().filter_map(|u| existe(u).err()).collect();
    // El segundo espejo de ffmpeg es solo de reserva: que caiga no es grave.
    let graves: Vec<&String> = fallos.iter().filter(|f| !f.contains("gyan.dev")).collect();
    assert!(graves.is_empty(), "direcciones rotas: {graves:#?}");
}

#[test]
fn el_motor_de_la_tarjeta_grafica_se_descarga_comprobado_y_arranca() {
    if !con_red() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let estado: setup::Shared = Mutex::new(State::default());
    setup::install(dir.path(), &[Tool::WhisperVulkan], &estado);
    {
        let s = estado.lock().unwrap();
        assert!(s.done && s.error.is_none(), "la instalacion fallo: {:?}\n{}", s.error, s.log.join("\n"));
    }
    assert!(setup::vulkan_installed(dir.path()));
    assert!(arranca(&setup::whisper_cli(dir.path(), "whisper-vulkan"), &["--help"]).to_lowercase().contains("usage"));
}
