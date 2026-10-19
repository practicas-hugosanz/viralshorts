//! Fase 2 con ffmpeg de verdad: audio, descarga de un fichero local y
//! transcripcion por zonas (con un motor de mentira, para no depender de un
//! modelo). Las pruebas con red (yt-dlp) solo corren con `VS_NET_TESTS=1`.

use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};

use viralshorts::audio_energy;
use viralshorts::config::{DownloadConfig, TranscribeConfig};
use viralshorts::download::{self, YtDlp};
use viralshorts::ffmpeg;
use viralshorts::transcribe::{Device, Engine, EngineOptions, EngineOutput, TranscribeError, Transcriber};
use viralshorts::words::{Segment, Word};

fn hay_ffmpeg() -> bool {
    ffmpeg::which("ffmpeg").is_some() && ffmpeg::which("ffprobe").is_some()
}

/// Audio sintetico: un tono a `volumen` durante `segundos`.
fn tono(destino: &Path, segundos: u32, filtro_volumen: &str) {
    let estado = Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
        .arg(format!("sine=frequency=300:duration={segundos}:sample_rate=16000"))
        .args(["-af", filtro_volumen, "-c:a", "aac"])
        .arg(destino)
        .status()
        .unwrap();
    assert!(estado.success());
}

#[test]
fn se_decodifica_el_numero_exacto_de_muestras() {
    if !hay_ffmpeg() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let audio = dir.path().join("a.m4a");
    tono(&audio, 3, "volume=0.5");

    let (mut total, mut trozos) = (0usize, 0usize);
    ffmpeg::decode_audio_chunks(&audio, 16_000, None, None, 10_000, |c| {
        assert!(c.iter().all(|s| s.abs() <= 1.0));
        total += c.len();
        trozos += 1;
    })
    .unwrap();
    assert!((total as i64 - 48_000).abs() < 2_500, "muestras {total}"); // el AAC rellena un poco
    assert!(trozos >= 4);

    // Solo un tramo.
    let mut tramo = 0usize;
    ffmpeg::decode_audio_chunks(&audio, 16_000, Some(1.0), Some(1.0), 4_000, |c| tramo += c.len()).unwrap();
    assert!((tramo as i64 - 16_000).abs() < 1_500, "tramo {tramo}");
}

#[test]
fn la_energia_distingue_lo_alto_de_lo_bajo() {
    if !hay_ffmpeg() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let audio = dir.path().join("a.m4a");
    // 20 s: callado, con una subida fuerte entre los segundos 8 y 12.
    tono(&audio, 20, "volume='if(between(t,8,12),0.9,0.02)':eval=frame");

    let pista = audio_energy::analyse(&audio, 20.0).unwrap();
    assert!((pista.level.len() as i64 - 80).abs() <= 2, "muestras {}", pista.level.len());
    assert!(pista.level.iter().all(|l| (0.0..=1.0).contains(l)));
    let fuerte = pista.window_score(8.0, 12.0);
    let flojo = pista.window_score(0.0, 4.0);
    assert!(fuerte > flojo + 0.3, "fuerte {fuerte} flojo {flojo}");
    assert!(pista.at(10.0) > pista.at(2.0));

    // El analisis de un tramo da una curva desplazada.
    let tramo = audio_energy::analyse_range(&audio, 6.0, 8.0).unwrap();
    assert_eq!(tramo.offset, 6.0);
    assert!(tramo.at(10.0) > tramo.at(7.0));
}

#[test]
fn un_fichero_local_se_usa_tal_cual() {
    if !hay_ffmpeg() {
        return;
    }
    struct SinRed;
    impl YtDlp for SinRed {
        fn download(&self, _: &str, _: &Path, _: &DownloadConfig) -> Result<serde_json::Value, download::DownloadError> {
            panic!("un fichero local no se descarga");
        }
        fn metadata(&self, _: &str, _: &DownloadConfig) -> Result<serde_json::Value, download::DownloadError> {
            panic!("un fichero local no pide metadatos");
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let audio = dir.path().join("Mi Podcast.m4a");
    tono(&audio, 4, "volume=0.5");
    // Ni con ffprobe ni con yt-dlp hace falta mas: el titulo sale del nombre.
    let video = Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc=size=320x240:rate=10:duration=4", "-i"])
        .arg(&audio)
        .args(["-c:v", "libx264", "-c:a", "copy", "-shortest"])
        .arg(dir.path().join("Mi Podcast.mp4"))
        .status()
        .unwrap();
    assert!(video.success());
    std::fs::write(
        dir.path().join("Mi Podcast.info.json"),
        r#"{"title": "El titulo de verdad", "heatmap": [{"start_time": 0, "end_time": 2, "value": 1.0}, {"start_time": 2, "end_time": 4, "value": 0.1}]}"#,
    )
    .unwrap();

    let local = dir.path().join("Mi Podcast.mp4");
    let fuente = download::fetch_with(&SinRed, local.to_str().unwrap(), &dir.path().join("work").join("podcast"), &DownloadConfig::default()).unwrap();
    assert_eq!(fuente.title, "El titulo de verdad");
    assert!((fuente.duration - 4.0).abs() < 0.3);
    assert!(fuente.has_heatmap());
    assert_eq!(fuente.path, local);
}

// -- transcripcion por zonas con un motor de mentira -------------------------------

struct MotorFalso {
    llamadas: Arc<Mutex<Vec<f64>>>,
}

impl Engine for MotorFalso {
    fn cuda_available(&self) -> bool {
        false
    }

    fn transcribe(&mut self, wav: &Path, device: Device, _o: &EngineOptions, _p: &mut dyn FnMut(f64)) -> Result<EngineOutput, TranscribeError> {
        assert_eq!(device, Device::Cpu);
        // WAV mono de 16 bits a 16 kHz: 44 bytes de cabecera y 32 000 por segundo.
        let bytes = std::fs::metadata(wav).map_err(|e| TranscribeError(e.to_string()))?.len();
        self.llamadas.lock().unwrap().push((bytes as f64 - 44.0) / 32_000.0);
        Ok(EngineOutput {
            language: Some("es".into()),
            segments: vec![Segment { start: 0.5, end: 2.0, text: "hola mundo".into(), words: vec![Word::new("hola", 0.5, 1.0), Word::new("mundo.", 1.0, 2.0)] }],
        })
    }
}

#[test]
fn las_zonas_se_transcriben_con_tiempos_absolutos_y_la_segunda_vez_sale_de_la_cache() {
    if !hay_ffmpeg() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let audio = dir.path().join("a.m4a");
    tono(&audio, 30, "volume=0.5");

    let llamadas = Arc::new(Mutex::new(Vec::new()));
    let cfg = TranscribeConfig { device: "auto".into(), ..Default::default() };
    let mut t = Transcriber::new(cfg.clone(), dir.path().join("cache"), Box::new(MotorFalso { llamadas: llamadas.clone() })).unwrap();

    let zonas = [(2.0, 8.0), (20.0, 26.0), (29.9, 30.0)]; // la ultima es demasiado corta y se ignora
    let transcripcion = t.transcribe_regions(&audio, 30.0, &zonas).unwrap();

    let palabras = transcripcion.words();
    assert_eq!(palabras.len(), 4);
    assert_eq!((palabras[0].start, palabras[0].text.as_str()), (2.5, "hola")); // 2.0 + 0.5
    assert_eq!(palabras[2].start, 20.5);
    assert!(palabras.windows(2).all(|p| p[0].start <= p[1].start));
    assert_eq!(transcripcion.language, "es");
    let duraciones = llamadas.lock().unwrap().clone();
    assert_eq!(duraciones.len(), 2);
    assert!(duraciones.iter().all(|d| (d - 6.0).abs() < 0.3), "{duraciones:?}");

    // Segunda vez: mismo video, mismas zonas, mismo ajuste -> cache.
    let mut otra = Transcriber::new(cfg, dir.path().join("cache"), Box::new(MotorFalso { llamadas: llamadas.clone() })).unwrap();
    assert_eq!(otra.transcribe_regions(&audio, 30.0, &zonas).unwrap(), transcripcion);
    assert_eq!(llamadas.lock().unwrap().len(), 2, "no debe volver a transcribir");

    // No quedan WAV temporales por el camino.
    let sobras: Vec<_> = std::fs::read_dir(dir.path().join("cache")).unwrap().filter_map(|e| e.ok()).filter(|e| e.file_name().to_string_lossy().ends_with(".wav")).collect();
    assert!(sobras.is_empty());
}

#[test]
fn un_audio_sin_habla_es_un_error_claro() {
    if !hay_ffmpeg() {
        return;
    }
    struct Mudo;
    impl Engine for Mudo {
        fn cuda_available(&self) -> bool {
            false
        }
        fn transcribe(&mut self, _: &Path, _: Device, _: &EngineOptions, _: &mut dyn FnMut(f64)) -> Result<EngineOutput, TranscribeError> {
            Ok(EngineOutput::default())
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let audio = dir.path().join("a.m4a");
    tono(&audio, 5, "volume=0.5");
    let mut t = Transcriber::new(TranscribeConfig::default(), dir.path().join("cache"), Box::new(Mudo)).unwrap();
    let error = t.transcribe_regions(&audio, 5.0, &[(0.0, 4.0)]).unwrap_err().to_string();
    assert!(error.contains("no devolvio ninguna palabra"), "{error}");
}

// -- con red -----------------------------------------------------------------------

#[test]
fn yt_dlp_de_verdad_pide_los_metadatos_de_un_video() {
    if std::env::var("VS_NET_TESTS").is_err() || ffmpeg::which("yt-dlp").is_none() {
        eprintln!("prueba con red omitida (VS_NET_TESTS=1 y yt-dlp en el PATH)");
        return;
    }
    // "Me at the zoo": el primer video de YouTube, 19 segundos.
    let info = download::YtDlpCli.metadata("https://www.youtube.com/watch?v=jNQXAC9IVRw", &DownloadConfig::default()).unwrap();
    assert_eq!(info["id"], "jNQXAC9IVRw");
    assert!(info["title"].as_str().unwrap().contains("zoo"));
    assert!((info["duration"].as_f64().unwrap() - 19.0).abs() < 2.0);
    assert!(info.get("formats").is_none(), "se guarda solo lo que se usa");
}

#[test]
fn yt_dlp_de_verdad_descarga_y_reutiliza() {
    if std::env::var("VS_NET_TESTS").is_err() || !hay_ffmpeg() || ffmpeg::which("yt-dlp").is_none() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("podcast");
    let cfg = DownloadConfig { max_height: 360, ..Default::default() };
    let url = "https://www.youtube.com/watch?v=jNQXAC9IVRw";

    let video = download::fetch(url, &base, &cfg).unwrap();
    assert!(video.path.is_file());
    assert!((video.duration - 19.0).abs() < 2.0);
    assert!(video.title.contains("zoo"));
    assert!(dir.path().join("podcast.info.json").is_file());

    let otra_vez = download::fetch(url, &base, &cfg).unwrap();
    assert_eq!(otra_vez.path, video.path);
}
