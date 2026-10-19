//! whisper.cpp de verdad sobre un audio en castellano. Necesita `bin/whisper`
//! (lo deja el instalador) y red la primera vez, para bajar el modelo `tiny`,
//! asi que solo corre con `VS_NET_TESTS=1`.

use std::path::PathBuf;

use viralshorts::config::TranscribeConfig;
use viralshorts::ffmpeg;
use viralshorts::transcribe::{Transcriber, WhisperCpp};

fn raiz() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..")
}

#[test]
fn transcribe_un_short_real_con_palabras_y_tiempos() {
    let video = raiz().join("output").join("short_1.mp4");
    if std::env::var("VS_NET_TESTS").is_err() || !video.is_file() || ffmpeg::which("ffmpeg").is_none() {
        eprintln!("prueba omitida (VS_NET_TESTS=1, output/short_1.mp4 y ffmpeg)");
        return;
    }
    let motor = match WhisperCpp::discover(&raiz()) {
        Ok(motor) => motor,
        Err(_) => {
            eprintln!("prueba omitida: no hay whisper-cli en bin/whisper");
            return;
        }
    };

    let duracion = ffmpeg::probe(&video).unwrap().duration;
    let cache = tempfile::tempdir().unwrap();
    let cfg = TranscribeConfig { model_size: "tiny".into(), language: Some("es".into()), ..Default::default() };
    let mut t = Transcriber::new(cfg, cache.path(), Box::new(motor)).unwrap();

    let transcripcion = t.transcribe_regions(&video, duracion, &[(0.0, duracion.min(30.0))]).unwrap();
    let palabras = transcripcion.words();
    let texto: Vec<&str> = palabras.iter().map(|w| w.text.as_str()).collect();
    eprintln!("idioma {} | {} palabras\n{}", transcripcion.language, palabras.len(), texto.join(" "));
    for w in palabras.iter().take(12) {
        eprintln!("  {:6.2}-{:6.2}  p={:.2}  {}", w.start, w.end, w.probability, w.text);
    }

    assert_eq!(transcripcion.language, "es");
    assert!(palabras.len() > 10, "muy pocas palabras: {}", palabras.len());
    assert!(palabras.windows(2).all(|p| p[0].start <= p[1].start + 1e-9), "palabras desordenadas");
    assert!(palabras.iter().all(|w| w.end >= w.start && w.start >= 0.0 && w.end <= duracion + 1.0));
    assert!(palabras.iter().all(|w| (0.0..=1.0).contains(&w.probability)));
    assert!(transcripcion.segments.iter().any(|s| s.words.iter().any(|w| w.ends_sentence())), "ni una frase terminada");
}
