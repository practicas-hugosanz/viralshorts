//! Curva de energia del audio.
//!
//! Sirve como proxy barato de "aqui pasa algo": risas, subidas de voz, gente
//! hablandose encima, golpes de mesa. Un tramo con energia alta y con picos
//! suele funcionar mejor que uno plano, aunque se digan las mismas palabras.

use std::path::Path;

use crate::ffmpeg::{self, FfmpegError};

const SAMPLE_RATE: u32 = 16_000;
pub const HOP_SECONDS: f64 = 0.25;

/// Nivel RMS normalizado (0..1) muestreado cada `hop` segundos.
#[derive(Debug, Clone)]
pub struct EnergyTrack {
    pub level: Vec<f32>,
    pub hop: f64,
    pub duration: f64,
    /// Instante del video al que corresponde la primera muestra: 0 cuando la
    /// curva cubre el video entero, el inicio del clip cuando solo cubre un
    /// tramo (ver `analyse_range`).
    pub offset: f64,
    peak_threshold: f32,
}

impl EnergyTrack {
    pub fn new(level: Vec<f32>, hop: f64, duration: f64, offset: f64) -> Self {
        let peak_threshold = percentile(&level, 80.0) as f32;
        Self { level, hop, duration, offset, peak_threshold }
    }

    /// Puntua un tramo combinando nivel medio y densidad de picos.
    pub fn window_score(&self, start: f64, end: f64) -> f64 {
        let lo = ((start / self.hop) as i64).max(0) as usize;
        let hi = (((end / self.hop) as i64).max(0) as usize).min(self.level.len());
        if hi < lo + 2 {
            return 0.0;
        }
        let chunk = &self.level[lo..hi];
        let n = chunk.len() as f64;
        let mean_level = chunk.iter().map(|&v| v as f64).sum::<f64>() / n;
        // Picos = fraccion de tiempo por encima del percentil 80 global. Un
        // tramo con muchos picos tiene mas "movimiento" que uno alto pero plano.
        let spikes = chunk.iter().filter(|&&v| v > self.peak_threshold).count() as f64 / n;
        0.6 * mean_level + 0.4 * spikes
    }

    /// Nivel (0..1) en un instante concreto. Fuera de rango devuelve 0.
    pub fn at(&self, moment: f64) -> f64 {
        let index = ((moment - self.offset) / self.hop).trunc();
        if index < 0.0 || index >= self.level.len() as f64 {
            return 0.0;
        }
        self.level[index as usize] as f64
    }
}

/// Extrae la curva de energia del audio completo de un fichero.
pub fn analyse(path: &Path, duration: f64) -> Result<EnergyTrack, FfmpegError> {
    log::info!("Analizando la energia del audio...");
    let track = analyse_inner(path, duration, None, None)?;
    if track.level.len() < 2 {
        log::warn!("Audio demasiado corto para analizar la energia.");
    } else {
        log::info!("Energia calculada: {} muestras ({HOP_SECONDS:.2}s por muestra).", track.level.len());
    }
    Ok(track)
}

/// Curva de energia de un solo tramo, sin decodificar el video entero. La usa
/// el reencuadre al montar desde un plan guardado, donde no se ha analizado el
/// audio completo.
pub fn analyse_range(path: &Path, start: f64, duration: f64) -> Result<EnergyTrack, FfmpegError> {
    analyse_inner(path, duration, Some(start), Some(duration))
}

fn analyse_inner(path: &Path, duration: f64, start: Option<f64>, span: Option<f64>) -> Result<EnergyTrack, FfmpegError> {
    let hop_samples = (SAMPLE_RATE as f64 * HOP_SECONDS) as usize;
    let mut rms_db: Vec<f64> = Vec::new();
    let mut carry: Vec<f32> = Vec::with_capacity(hop_samples);

    ffmpeg::decode_audio_chunks(path, SAMPLE_RATE, start, span, hop_samples * 64, |chunk| {
        for &sample in chunk {
            carry.push(sample);
            if carry.len() == hop_samples {
                rms_db.push(block_db(&carry));
                carry.clear();
            }
        }
    })?;

    Ok(track_from_db(&rms_db, duration, start.unwrap_or(0.0)))
}

/// dB del RMS de un bloque. En dB la percepcion de volumen es mucho mas lineal
/// que en amplitud.
fn block_db(block: &[f32]) -> f64 {
    let mean_square = block.iter().map(|&s| (s as f64) * (s as f64)).sum::<f64>() / block.len() as f64;
    20.0 * mean_square.sqrt().max(1e-6).log10()
}

/// Normaliza con percentiles en vez de min/max: un solo golpe de microfono no
/// debe aplastar toda la curva.
pub fn track_from_db(db: &[f64], duration: f64, offset: f64) -> EnergyTrack {
    if db.len() < 2 {
        return EnergyTrack::new(vec![0.0], HOP_SECONDS, duration, offset);
    }
    let lo = percentile(db, 5.0);
    let hi = percentile(db, 97.0);
    let level = db.iter().map(|&d| ((d - lo) / (hi - lo).max(1e-6)).clamp(0.0, 1.0) as f32).collect();
    EnergyTrack::new(level, HOP_SECONDS, duration, offset)
}

/// Percentil con interpolacion lineal, como `numpy.percentile`.
pub fn percentile<T: Copy + Into<f64>>(values: &[T], q: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted: Vec<f64> = values.iter().map(|&v| v.into()).collect();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let position = (sorted.len() - 1) as f64 * q / 100.0;
    let below = position.floor() as usize;
    let above = (below + 1).min(sorted.len() - 1);
    sorted[below] + (sorted[above] - sorted[below]) * (position - below as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentil_como_numpy() {
        // numpy.percentile([1, 2, 3, 4, 5], q): 1.8, 3.0, 4.88
        let v = [5.0, 1.0, 3.0, 2.0, 4.0];
        assert!((percentile(&v, 20.0) - 1.8).abs() < 1e-12);
        assert_eq!(percentile(&v, 50.0), 3.0);
        assert!((percentile(&v, 97.0) - 4.88).abs() < 1e-12);
        assert_eq!(percentile::<f64>(&[], 50.0), 0.0);
        assert_eq!(percentile(&[7.0], 90.0), 7.0);
    }

    #[test]
    fn la_curva_va_de_0_a_1_y_un_pico_aislado_no_la_aplasta() {
        // 100 bloques a -40 dB con un golpe de microfono a 0 dB.
        let mut db = vec![-40.0; 100];
        db[50] = 0.0;
        for (i, d) in db.iter_mut().enumerate() {
            *d += (i % 10) as f64; // algo de variacion
        }
        let t = track_from_db(&db, 25.0, 0.0);
        assert!(t.level.iter().all(|&l| (0.0..=1.0).contains(&l)));
        assert_eq!(t.level[50], 1.0);
        let medios = t.level.iter().filter(|&&l| l > 0.4).count();
        assert!(medios >= 20, "el golpe aplasto el resto de la curva: {medios}");
    }

    #[test]
    fn audio_demasiado_corto() {
        let t = track_from_db(&[-30.0], 0.2, 5.0);
        assert_eq!(t.level, vec![0.0]);
        assert_eq!(t.window_score(0.0, 10.0), 0.0);
        assert_eq!(t.offset, 5.0);
    }

    #[test]
    fn un_tramo_con_picos_puntua_mas_que_uno_plano() {
        let mut level = vec![0.5f32; 400];
        for i in (200..300).step_by(2) {
            level[i] = 1.0;
        }
        let t = EnergyTrack::new(level, 0.25, 100.0, 0.0);
        let plano = t.window_score(0.0, 25.0);
        let movido = t.window_score(50.0, 75.0);
        assert!(movido > plano, "{movido} <= {plano}");
    }

    #[test]
    fn puntuar_fuera_de_rango() {
        let t = EnergyTrack::new(vec![0.5; 10], 0.25, 2.5, 0.0);
        assert_eq!(t.window_score(100.0, 200.0), 0.0);
        assert_eq!(t.window_score(1.0, 1.1), 0.0);
        assert!(t.window_score(0.0, 100.0) > 0.0);
    }

    #[test]
    fn nivel_en_un_instante_respeta_el_desplazamiento() {
        let t = EnergyTrack::new(vec![0.1, 0.2, 0.3], 0.25, 0.75, 10.0);
        assert_eq!(t.at(9.5), 0.0);
        assert!((t.at(10.3) - 0.2).abs() < 1e-6);
        assert_eq!(t.at(11.0), 0.0);
    }

    #[test]
    fn el_bloque_en_decibelios() {
        assert!((block_db(&[0.1; 100]) + 20.0).abs() < 1e-6);
        assert!((block_db(&[0.0; 100]) + 120.0).abs() < 1e-6, "el silencio se acota a -120 dB");
    }
}
