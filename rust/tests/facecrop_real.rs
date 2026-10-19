//! YuNet de verdad (onnxruntime) sobre un podcast real. Necesita el video de
//! `work/podcast.mp4` y el modelo en `bin/models`; sin ellos se salta.
//!
//! El video es el de los shorts de `output/`, y la version Python ya midio esos
//! mismos tramos con OpenCV: guarda sus trayectorias en `work/cache`. Aqui se
//! comprueba que las de Rust van en la misma zona del plano.

use std::path::PathBuf;

use viralshorts::config::ReframeConfig;
use viralshorts::facecrop;

fn raiz() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..")
}

#[test]
fn yunet_encuadra_al_invitado_como_lo_hacia_opencv() {
    let video = raiz().join("work").join("podcast.mp4");
    let modelo = raiz().join("bin").join("models").join("face_detection_yunet_2023mar.onnx");
    if !video.is_file() || !modelo.is_file() {
        eprintln!("prueba omitida: falta work/podcast.mp4 o el modelo YuNet");
        return;
    }

    let cfg = ReframeConfig::default();
    let mut hallados = 0;
    for n in 1..=3 {
        let json = raiz().join("output").join(format!("short_{n}.json"));
        let Ok(texto) = std::fs::read_to_string(&json) else { continue };
        let datos: serde_json::Value = serde_json::from_str(&texto).unwrap();
        let (inicio, fin) = (datos["fuente"]["inicio"].as_f64().unwrap(), datos["fuente"]["fin"].as_f64().unwrap());

        let empezo = std::time::Instant::now();
        let pista = facecrop::track_faces(&video, inicio, fin - inicio, &cfg, None, None);
        eprintln!("short_{n} ({inicio:.1}-{fin:.1}): {:.1}s -> {:?}", empezo.elapsed().as_secs_f64(), pista);

        let pista = pista.unwrap_or_else(|| panic!("short_{n}: no se detecto a nadie"));
        assert!(pista.detection_ratio > 0.8, "short_{n}: deteccion {}", pista.detection_ratio);
        assert!(pista.points.iter().all(|(_, cx)| (0.1..0.4).contains(cx)), "short_{n}: se fue de la zona del invitado: {:?}", pista.points);
        assert!(pista.points.windows(2).all(|w| w[0].0 < w[1].0));
        hallados += 1;
    }
    assert!(hallados > 0, "no hay output/short_N.json con el que comparar");
}
