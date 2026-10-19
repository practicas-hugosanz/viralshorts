//! Eliminacion de silencios (jump cuts).
//!
//! Un podcast tiene pausas de uno o dos segundos cada pocas frases. En un video
//! de 45 segundos eso son 5-8 segundos de nada. Aqui se recortan los silencios
//! largos y, muy importante, se devuelve la funcion que traslada cualquier
//! instante del clip original al clip ya recortado: sin ella los subtitulos
//! quedarian desincronizados.

use crate::config::JumpCutsConfig;
use crate::words::Word;

pub type Interval = (f64, f64);

/// Tramos que se conservan, en la linea de tiempo del clip original.
#[derive(Debug, Clone, PartialEq)]
pub struct JumpCutPlan {
    pub keeps: Vec<Interval>,
    pub source_duration: f64,
}

impl JumpCutPlan {
    pub fn full(duration: f64) -> Self {
        Self { keeps: vec![(0.0, duration)], source_duration: duration }
    }

    pub fn enabled(&self) -> bool {
        self.keeps.len() > 1
    }

    pub fn output_duration(&self) -> f64 {
        self.keeps.iter().map(|(start, end)| end - start).sum()
    }

    pub fn removed(&self) -> f64 {
        self.source_duration - self.output_duration()
    }

    /// Traslada un instante del clip original al clip ya recortado.
    pub fn remap(&self, t: f64) -> f64 {
        let mut elapsed = 0.0;
        for &(start, end) in &self.keeps {
            if t < start {
                return elapsed; // cae en un hueco eliminado
            }
            if t <= end {
                return elapsed + (t - start);
            }
            elapsed += end - start;
        }
        elapsed
    }

    /// Reajusta las marcas de tiempo de las palabras al clip recortado.
    pub fn remap_words(&self, words: &[Word]) -> Vec<Word> {
        words
            .iter()
            .map(|word| {
                let start = self.remap(word.start);
                let mut end = self.remap(word.end);
                if end - start < 0.02 {
                    // La palabra cayo entera dentro de un hueco eliminado; una
                    // duracion minima evita que desaparezca del subtitulo.
                    end = start + 0.08;
                }
                Word { text: word.text.clone(), start, end, probability: word.probability }
            })
            .collect()
    }
}

/// Calcula que tramos del clip se conservan. `words` debe venir ya en
/// coordenadas relativas al clip (0 = inicio).
pub fn plan(words: &[Word], duration: f64, cfg: &JumpCutsConfig) -> JumpCutPlan {
    let full = JumpCutPlan::full(duration);
    if !cfg.enabled || words.is_empty() {
        return full;
    }

    // 1. Un intervalo por palabra, con margen para no cortar ataques ni colas.
    let spoken = merge(
        words
            .iter()
            .map(|w| ((w.start - cfg.padding).max(0.0), (w.end + cfg.padding).min(duration)))
            .collect(),
    );

    // 2. Hasta `max_silence` de silencio a cada lado de cada bloque hablado:
    //    quitar las pausas del todo suena atropellado e inhumano.
    let half = cfg.max_silence / 2.0;
    let keeps: Vec<Interval> = merge(
        spoken
            .iter()
            .map(|&(start, end)| ((start - half).max(0.0), (end + half).min(duration)))
            .collect(),
    )
    .into_iter()
    // 3. Fuera los trozos residuales demasiado cortos para verse como un plano.
    .filter(|(a, b)| b - a >= cfg.min_keep)
    .collect();

    if keeps.is_empty() {
        log::warn!("Los jump cuts se comerian el clip entero; se desactivan aqui.");
        return full;
    }

    let result = JumpCutPlan { keeps, source_duration: duration };
    if result.removed() < 0.5 {
        // Menos de medio segundo no compensa el coste extra de filtrado.
        return full;
    }

    log::info!(
        "Jump cuts: {} tramos, se quitan {:.1}s de silencio ({:.0}% -> {:.1}s).",
        result.keeps.len(),
        result.removed(),
        100.0 * result.removed() / duration,
        result.output_duration()
    );
    result
}

fn merge(mut intervals: Vec<Interval>) -> Vec<Interval> {
    intervals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mut merged: Vec<Interval> = Vec::with_capacity(intervals.len());
    for (start, end) in intervals {
        match merged.last_mut() {
            Some(last) if start <= last.1 => last.1 = last.1.max(end),
            _ => merged.push((start, end)),
        }
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(spans: &[(f64, f64)]) -> Vec<Word> {
        spans.iter().enumerate().map(|(i, &(a, b))| Word::new(format!("p{i}"), a, b)).collect()
    }

    #[test]
    fn sin_silencios_no_toca_nada() {
        let cfg = JumpCutsConfig::default();
        let r = plan(&words(&[(0.0, 1.0), (1.1, 2.0), (2.1, 3.0)]), 3.0, &cfg);
        assert!(!r.enabled());
        assert!((r.output_duration() - 3.0).abs() < 1e-9);
    }

    #[test]
    fn quita_el_silencio_de_en_medio() {
        let r = plan(&words(&[(0.0, 2.0), (8.0, 10.0)]), 10.0, &JumpCutsConfig::default());
        assert!(r.enabled());
        assert!(r.output_duration() < 10.0);
        assert!(r.removed() > 4.0);
    }

    #[test]
    fn remap_es_monotono_y_no_pasa_de_la_duracion() {
        let r = plan(&words(&[(0.0, 2.0), (8.0, 10.0)]), 10.0, &JumpCutsConfig::default());
        let mut anterior = -1.0;
        for decima in 0..=100 {
            let momento = r.remap(decima as f64 / 10.0);
            assert!(momento >= anterior - 1e-9);
            assert!(momento <= r.output_duration() + 1e-6);
            anterior = momento;
        }
    }

    #[test]
    fn las_palabras_conservan_el_orden_tras_recortar() {
        let habla = words(&[(0.0, 1.0), (1.2, 2.0), (9.0, 9.8)]);
        let r = plan(&habla, 10.0, &JumpCutsConfig::default());
        let movidas = r.remap_words(&habla);

        assert_eq!(movidas.len(), habla.len());
        for par in movidas.windows(2) {
            assert!(par[0].start <= par[1].start + 1e-9);
        }
        // Ninguna palabra puede quedarse sin duracion: desapareceria del
        // subtitulo sin que nadie se entere.
        assert!(movidas.iter().all(|w| w.end - w.start > 0.0));
    }

    #[test]
    fn un_clip_sin_palabras_no_se_borra_entero() {
        let r = plan(&[], 12.0, &JumpCutsConfig::default());
        assert!((r.output_duration() - 12.0).abs() < 1e-9);
    }

    #[test]
    fn desactivado_no_corta() {
        let cfg = JumpCutsConfig { enabled: false, ..Default::default() };
        let r = plan(&words(&[(0.0, 1.0), (9.0, 10.0)]), 10.0, &cfg);
        assert!(!r.enabled());
    }

    #[test]
    fn un_hueco_eliminado_colapsa_al_punto_de_corte() {
        let r = JumpCutPlan { keeps: vec![(0.0, 5.0), (7.0, 12.0)], source_duration: 12.0 };
        assert_eq!(r.remap(6.0), 5.0);
        assert_eq!(r.remap(7.0), 5.0);
        assert_eq!(r.remap(8.0), 6.0);
        assert_eq!(r.remap(99.0), 10.0);
    }

    #[test]
    fn la_palabra_dentro_de_un_hueco_conserva_una_duracion_minima() {
        let r = JumpCutPlan { keeps: vec![(0.0, 5.0), (7.0, 12.0)], source_duration: 12.0 };
        let w = r.remap_words(&[Word::new("x", 5.5, 6.5)]);
        assert!((w[0].end - w[0].start - 0.08).abs() < 1e-9);
    }

    #[test]
    fn la_union_de_intervalos() {
        assert_eq!(merge(vec![(3.0, 4.0), (0.0, 1.0), (0.5, 2.0)]), vec![(0.0, 2.0), (3.0, 4.0)]);
        assert!(merge(vec![]).is_empty());
    }
}
