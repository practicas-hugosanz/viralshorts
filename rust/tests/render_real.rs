//! Renderiza un short de verdad con el ffmpeg instalado.
//!
//! Los tests unitarios comprueban el texto del grafo; esto comprueba que ESTA
//! version de ffmpeg lo acepta y que la salida tiene la forma que promete:
//! 1080x1920, con audio, y de la duracion que dice el plan de jump cuts.
//! Se salta si no hay ffmpeg.

use std::path::Path;
use std::process::Command;

use viralshorts::config::Config;
use viralshorts::ffmpeg;
use viralshorts::jumpcuts::JumpCutPlan;
use viralshorts::render::{extract_thumbnail, render, RenderRequest};
use viralshorts::words::Track;

fn hay_ffmpeg() -> bool {
    ffmpeg::which("ffmpeg").is_some() && ffmpeg::which("ffprobe").is_some()
}

fn generar(destino: &Path, fuente: &str, tono: u32, segundos: u32) {
    let estado = Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
        .arg(format!("{fuente}=size=1280x720:rate=25:duration={segundos}"))
        .args(["-f", "lavfi", "-i"])
        .arg(format!("sine=frequency={tono}:duration={segundos}"))
        .args(["-c:v", "libx264", "-pix_fmt", "yuv420p", "-c:a", "aac", "-shortest"])
        .arg(destino)
        .status()
        .expect("lanzar ffmpeg");
    assert!(estado.success());
}

fn peticion(dir: &Path, plan: JumpCutPlan, track: Option<Track>, nombre: &str) -> RenderRequest {
    RenderRequest {
        podcast: dir.join("podcast.mp4"),
        background: dir.join("fondo.mp4"),
        start: 1.0,
        end: 7.0,
        background_offset: 0.5,
        plan,
        track,
        ass_name: None,
        output: dir.join(nombre),
        work_dir: dir.join("work"),
        beeps: Vec::new(),
        whooshes: Vec::new(),
        emojis: Vec::new(),
    }
}

fn configuracion_rapida() -> Config {
    let mut cfg = Config::default();
    cfg.audio.loudnorm = false;
    cfg.output.hardware_encoder = "off".into();
    cfg.output.preset = "ultrafast".into();
    cfg
}

#[test]
fn el_short_sale_vertical_con_audio_y_la_duracion_del_plan() {
    if !hay_ffmpeg() {
        eprintln!("sin ffmpeg: prueba omitida");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    generar(&dir.path().join("podcast.mp4"), "testsrc2", 440, 10);
    generar(&dir.path().join("fondo.mp4"), "smptebars", 880, 3); // mas corto que el clip: se repite
    let cfg = configuracion_rapida();

    let sin_cortes = peticion(dir.path(), JumpCutPlan::full(6.0), None, "a.mp4");
    let salida = render(&sin_cortes, &cfg).expect("render sin cortes");
    let info = ffmpeg::probe(&salida).unwrap();
    assert_eq!((info.width, info.height), (1080, 1920));
    assert!(info.has_audio);
    assert!((info.duration - 6.0).abs() < 0.3, "duracion {}", info.duration);

    // Con jump cuts y una trayectoria de cara que cruza un hueco eliminado.
    let plan = JumpCutPlan { keeps: vec![(0.0, 2.0), (4.0, 6.0)], source_duration: 6.0 };
    let pista = Track::new(vec![(0.0, 0.3), (3.0, 0.3), (4.0, 0.7), (6.0, 0.7)], 1.0);
    let con_cortes = peticion(dir.path(), plan, Some(pista), "b.mp4");
    let salida = render(&con_cortes, &cfg).expect("render con cortes y seguimiento");
    let info = ffmpeg::probe(&salida).unwrap();
    assert_eq!((info.width, info.height), (1080, 1920));
    assert!((info.duration - 4.0).abs() < 0.3, "duracion {}", info.duration);
}

#[test]
fn la_normalizacion_de_volumen_y_el_zoom_los_acepta_ffmpeg() {
    if !hay_ffmpeg() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    generar(&dir.path().join("podcast.mp4"), "testsrc2", 440, 10);
    generar(&dir.path().join("fondo.mp4"), "smptebars", 880, 10);
    let mut cfg = configuracion_rapida();
    cfg.audio.loudnorm = true;
    cfg.zoom.enabled = true;

    let salida = render(&peticion(dir.path(), JumpCutPlan::full(6.0), None, "c.mp4"), &cfg).expect("render con loudnorm y zoom");
    assert!(salida.is_file());
}

#[test]
fn la_portada_se_saca_del_short() {
    if !hay_ffmpeg() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    generar(&dir.path().join("short_1.mp4"), "testsrc2", 440, 3);
    let portada = extract_thumbnail(&dir.path().join("short_1.mp4"), 1.2);
    assert!(portada.is_some_and(|p| p.is_file()));
}

#[test]
fn un_ffmpeg_que_falla_devuelve_el_motivo() {
    if !hay_ffmpeg() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let args = ["-i".to_string(), dir.path().join("no_existe.mp4").display().to_string(), "-f".into(), "null".into(), "-".into()];
    let error = ffmpeg::run(&args, None, None, "x").unwrap_err().to_string();
    assert!(error.contains("ffmpeg fallo") && error.contains("no_existe"), "{error}");
}
