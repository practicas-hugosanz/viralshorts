//! Tipos compartidos de la transcripcion y del reencuadre.

use serde::{Deserialize, Serialize};

const SENTENCE_ENDINGS: [char; 4] = ['.', '!', '?', '…'];

fn one() -> f64 {
    1.0
}

/// Una palabra con su marca de tiempo.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Word {
    /// Texto tal cual lo devuelve Whisper, con puntuacion.
    pub text: String,
    pub start: f64,
    pub end: f64,
    #[serde(default = "one")]
    pub probability: f64,
    /// Quien la dice cuando hay dos personas en plano: 0 la de la izquierda,
    /// 1 la de la derecha. Sirve para dar a cada una su color en los subtitulos.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speaker: Option<u8>,
}

impl Word {
    pub fn new(text: impl Into<String>, start: f64, end: f64) -> Self {
        Self { text: text.into(), start, end, probability: 1.0, speaker: None }
    }

    pub fn ends_sentence(&self) -> bool {
        self.text.trim().chars().last().is_some_and(|c| SENTENCE_ENDINGS.contains(&c))
    }

    pub fn shifted(&self, offset: f64) -> Word {
        Word {
            text: self.text.clone(),
            start: self.start + offset,
            end: self.end + offset,
            probability: self.probability,
            speaker: self.speaker,
        }
    }
}

/// Trayectoria del centro horizontal del encuadre, normalizada 0..1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Track {
    /// (t relativo al clip, cx normalizado)
    pub points: Vec<(f64, f64)>,
    /// Fraccion de muestras con cara detectada.
    pub detection_ratio: f64,
    /// Si hay dos personas en plano a la vez, el centro horizontal de cada
    /// una (izquierda, derecha): permite la pantalla partida.
    #[serde(default)]
    pub duo: Option<(f64, f64)>,
    /// Quien habla desde cada instante (t relativo al clip, 0 izquierda / 1 derecha).
    #[serde(default)]
    pub speakers: Vec<(f64, u8)>,
}

impl Track {
    pub fn new(points: Vec<(f64, f64)>, detection_ratio: f64) -> Self {
        Self { points, detection_ratio, duo: None, speakers: Vec::new() }
    }

    /// Quien habla en el instante `t` (segundos del clip, antes de quitar silencios).
    pub fn speaker_at(&self, t: f64) -> Option<u8> {
        self.speakers.iter().take_while(|(at, _)| *at <= t).last().or(self.speakers.first()).map(|(_, who)| *who)
    }

    pub fn is_useful(&self) -> bool {
        self.points.len() >= 2 && self.detection_ratio >= 0.15
    }
}

/// Un tramo de habla con sus palabras.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Segment {
    pub start: f64,
    pub end: f64,
    pub text: String,
    pub words: Vec<Word>,
}

/// Resultado de transcribir un video (o solo algunas zonas de el). Se guarda
/// en la cache con exactamente esta forma.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Transcript {
    pub language: String,
    pub duration: f64,
    pub segments: Vec<Segment>,
}

impl Transcript {
    pub fn iter_words(&self) -> impl Iterator<Item = &Word> {
        self.segments.iter().flat_map(|s| s.words.iter())
    }

    pub fn words(&self) -> Vec<Word> {
        self.iter_words().cloned().collect()
    }

    /// Palabras cuyo centro cae dentro de [start, end).
    pub fn words_between(&self, start: f64, end: f64) -> Vec<&Word> {
        self.iter_words()
            .filter(|w| {
                let centre = (w.start + w.end) / 2.0;
                start <= centre && centre < end
            })
            .collect()
    }

    pub fn text_between(&self, start: f64, end: f64) -> String {
        self.words_between(start, end).iter().map(|w| w.text.trim()).collect::<Vec<_>>().join(" ").trim().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn el_texto_de_un_tramo_usa_el_centro_de_cada_palabra() {
        let t = Transcript {
            language: "es".into(),
            duration: 10.0,
            segments: vec![Segment {
                start: 0.0,
                end: 6.0,
                text: "a b c".into(),
                words: vec![Word::new(" a", 0.0, 1.0), Word::new("b", 1.9, 3.1), Word::new("c", 5.0, 6.0)],
            }],
        };
        assert_eq!(t.text_between(0.0, 3.0), "a b");
        assert_eq!(t.text_between(2.4, 10.0), "b c");
        assert_eq!(t.text_between(6.0, 9.0), "");
        assert_eq!(t.words().len(), 3);
    }

    #[test]
    fn la_cache_conserva_la_forma_de_python() {
        let t = Transcript {
            language: "es".into(),
            duration: 3.0,
            segments: vec![Segment { start: 0.0, end: 1.0, text: "hola".into(), words: vec![Word::new("hola", 0.0, 1.0)] }],
        };
        let json = serde_json::to_value(&t).unwrap();
        assert_eq!(json["segments"][0]["words"][0]["probability"], 1.0);
        assert_eq!(serde_json::from_value::<Transcript>(json).unwrap(), t);
    }

    #[test]
    fn fin_de_frase() {
        assert!(Word::new("hola.", 0.0, 1.0).ends_sentence());
        assert!(Word::new(" que…  ", 0.0, 1.0).ends_sentence());
        assert!(!Word::new("hola,", 0.0, 1.0).ends_sentence());
        assert!(!Word::new("", 0.0, 1.0).ends_sentence());
    }

    #[test]
    fn desplazar_conserva_el_resto() {
        let w = Word { text: "x".into(), start: 1.0, end: 2.0, probability: 0.5, speaker: Some(1) }.shifted(-0.5);
        assert_eq!((w.start, w.end, w.probability), (0.5, 1.5, 0.5));
    }

    #[test]
    fn una_trayectoria_sirve_con_dos_puntos_y_deteccion_suficiente() {
        let t = |n, r| Track::new(vec![(0.0, 0.5); n], r);
        assert!(t(2, 0.15).is_useful());
        assert!(!t(1, 1.0).is_useful());
        assert!(!t(5, 0.1).is_useful());
    }

    #[test]
    fn la_probabilidad_es_opcional_al_leer_planes() {
        let w: Word = serde_json::from_str(r#"{"text":"a","start":0,"end":1}"#).unwrap();
        assert_eq!(w.probability, 1.0);
    }
}
