//! Subtitulos en otro idioma.
//!
//! Dos caminos:
//!
//! - **A ingles**: whisper traduce el mismo audio (`-tr`). No hace falta
//!   conexion y cada palabra sale con su momento real, como al transcribir.
//! - **A cualquier otro idioma**: Claude traduce frase a frase y cada frase
//!   traducida ocupa el mismo tramo de tiempo que la original; dentro de la
//!   frase, las palabras se reparten el tiempo segun lo que miden.

use crate::words::Word;

/// Idiomas que ofrece la interfaz: (codigo, nombre en espanol, nombre en ingles).
pub const LANGUAGES: &[(&str, &str, &str)] = &[
    ("en", "inglés", "English"),
    ("es", "español", "Spanish"),
    ("fr", "francés", "French"),
    ("pt", "portugués", "Portuguese"),
    ("de", "alemán", "German"),
    ("it", "italiano", "Italian"),
];

pub fn language_name(code: &str) -> Option<&'static str> {
    LANGUAGES.iter().find(|(c, _, _)| *c == code).map(|(_, name, _)| *name)
}

pub fn english_name(code: &str) -> Option<&'static str> {
    LANGUAGES.iter().find(|(c, _, _)| *c == code).map(|(_, _, name)| *name)
}

/// Parte las palabras en frases para traducirlas enteras (traducir palabra a
/// palabra no tiene sentido). Una frase acaba en punto, en una pausa larga o a
/// las `max_words` palabras. Devuelve (desde, hasta) en indices de `words`.
pub fn phrases(words: &[Word], max_words: usize) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut start = 0;
    for (i, word) in words.iter().enumerate() {
        let gap = words.get(i + 1).map_or(0.0, |next| next.start - word.end);
        if word.ends_sentence() || gap > 0.8 || i + 1 - start >= max_words || i + 1 == words.len() {
            out.push((start, i + 1));
            start = i + 1;
        }
    }
    out
}

/// Coloca una frase traducida en el tramo de tiempo de la original. Cada
/// palabra recibe un trozo proporcional a su longitud (las largas tardan mas
/// en decirse) y hereda quien habla de la primera palabra original.
pub fn place(original: &[Word], translated: &str) -> Vec<Word> {
    let (Some(first), Some(last)) = (original.first(), original.last()) else { return Vec::new() };
    let pieces: Vec<&str> = translated.split_whitespace().collect();
    if pieces.is_empty() {
        return Vec::new();
    }
    let span = (last.end - first.start).max(0.05 * pieces.len() as f64);
    let weights: Vec<f64> = pieces.iter().map(|p| p.chars().count().max(2) as f64).collect();
    let total: f64 = weights.iter().sum();
    let mut cursor = first.start;
    pieces
        .iter()
        .zip(&weights)
        .map(|(piece, weight)| {
            let length = span * weight / total;
            let word = Word { text: (*piece).to_string(), start: cursor, end: cursor + length, probability: 1.0, speaker: first.speaker };
            cursor += length;
            word
        })
        .collect()
}

/// Las palabras traducidas de un clip entero a partir de las frases ya
/// traducidas (en el mismo orden que `phrases`). Si falta alguna traduccion se
/// deja esa frase como estaba: mejor eso que un hueco sin subtitulos.
pub fn assemble(words: &[Word], spans: &[(usize, usize)], translations: &[Option<String>]) -> Vec<Word> {
    spans
        .iter()
        .enumerate()
        .flat_map(|(i, &(from, to))| match translations.get(i).and_then(|t| t.as_deref()).filter(|t| !t.trim().is_empty()) {
            Some(text) => place(&words[from..to], text),
            None => words[from..to].to_vec(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(t: &str, a: f64, b: f64) -> Word {
        Word::new(t, a, b)
    }

    #[test]
    fn las_frases_se_cortan_en_el_punto_en_las_pausas_y_por_longitud() {
        let palabras = [w("Hola", 0.0, 0.3), w("chicos.", 0.3, 0.6), w("Hoy", 0.7, 0.9), w("vamos", 2.0, 2.3), w("a", 2.3, 2.4), w("jugar", 2.4, 2.8)];
        assert_eq!(phrases(&palabras, 12), [(0, 2), (2, 3), (3, 6)]);
        assert_eq!(phrases(&palabras[3..], 2), [(0, 2), (2, 3)]);
        assert!(phrases(&[], 12).is_empty());
    }

    #[test]
    fn la_frase_traducida_ocupa_el_mismo_tiempo_y_reparte_por_longitud() {
        let mut original = vec![w("Hola", 1.0, 1.4), w("a", 1.5, 1.6), w("todos", 1.6, 2.0)];
        original[0].speaker = Some(1);
        let puesta = place(&original, "Hello everybody");
        assert_eq!(puesta.len(), 2);
        assert!((puesta[0].start - 1.0).abs() < 1e-9 && (puesta[1].end - 2.0).abs() < 1e-9);
        assert!(puesta[1].end - puesta[1].start > puesta[0].end - puesta[0].start, "la larga dura mas");
        assert!(puesta.iter().all(|p| p.speaker == Some(1)));
        assert!(place(&original, "   ").is_empty());
    }

    #[test]
    fn lo_que_no_se_tradujo_se_queda_como_estaba() {
        let palabras = [w("Hola.", 0.0, 0.5), w("Adiós.", 1.0, 1.5)];
        let tramos = phrases(&palabras, 12);
        let texto: Vec<String> = assemble(&palabras, &tramos, &[Some("Hello.".into()), None]).into_iter().map(|w| w.text).collect();
        assert_eq!(texto, ["Hello.", "Adiós."]);
    }

    #[test]
    fn nombres_de_idiomas() {
        assert_eq!((language_name("en"), english_name("pt"), language_name("xx")), (Some("inglés"), Some("Portuguese"), None));
    }
}
