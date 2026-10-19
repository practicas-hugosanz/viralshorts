//! Cuanto se entiende un clip sin haber visto el resto del podcast.
//!
//! Un clip puede estar en el minuto mas rebobinado del video y aun asi ser
//! inservible, porque empieza en "...y entonces eso es justo lo que le paso a
//! el". El heatmap dice DONDE esta lo bueno; no dice donde EMPIEZA.
//!
//! Aqui se puntua lo autocontenido que es el arranque de cada ventana, con dos
//! ideas independientes:
//!
//! 1. Fronteras de tema (TextTiling). Dos tramos seguidos que comparten
//!    vocabulario hablan de lo mismo; cuando el vocabulario cambia de golpe, ahi
//!    ha empezado un tema nuevo.
//! 2. Arranque anaforico. "Eso", "el", "por eso", "como te decia": palabras que
//!    se refieren a algo que el espectador no ha visto.
//!
//! Ninguna necesita un modelo de lenguaje.

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use unicode_normalization::UnicodeNormalization;

use crate::config::ContextConfig;
use crate::scoring::Window;
use crate::words::Transcript;

/// Marcadores que se refieren a algo dicho ANTES. Empezar un clip por aqui es
/// empezar por la mitad.
const ANAPHORIC: &[&str] = &[
    "eso", "esos", "esas", "aquello", "aquel", "aquella", "ese", "esa",
    "entonces", "por eso", "por ello", "asi que", "y por eso", "o sea",
    "es decir", "total que", "en fin", "resumiendo", "como decia",
    "como te decia", "como deciamos", "lo que decia", "lo que te decia",
    "volviendo", "ademas", "tambien", "igualmente", "encima", "y claro",
    "claro que", "pero claro", "y eso", "y ahi", "ahi es donde", "ahi fue",
    "de ahi", "el mismo", "la misma", "lo mismo", "otra vez", "de nuevo",
    "este tio", "esta tia", "este hombre", "esta mujer", "el tio", "la tia",
    "y el", "y ella", "y ellos", "le dije", "le dijo", "me dijo", "me decia",
    "pero ", "y ", "porque ", "sino", "aunque", "es que", "si es que",
    "so ", "then ", "and then", "that guy", "that thing", "as i said",
    "like i said", "anyway", "also", "besides", "again",
];

/// Empezar preguntando o presentando SI es un buen arranque: el que pregunta
/// suele estar abriendo tema, y la pregunta le da el contexto al espectador.
const OPENING: &[&str] = &[
    "que es", "que son", "cual es", "por que", "como es", "como se",
    "sabes que", "sabias que", "te acuerdas", "os acordais", "imagina",
    "hay una", "hay un", "existe", "el otro dia", "una vez", "cuando yo",
    "cuando era", "mi ", "en ", "la gente", "todo el mundo", "resulta que",
    "what is", "what are", "why do", "how do", "did you know", "there is",
    "there was", "one day", "when i",
];

fn stop_words() -> &'static HashSet<&'static str> {
    static SET: OnceLock<HashSet<&'static str>> = OnceLock::new();
    SET.get_or_init(|| {
        "que de la el los las un una unos unas y o pero porque como para por con sin sobre
         entre cuando donde esto eso este esta estos estas muy mas menos tan todo toda todos todas
         nada algo cada otro otra ser estar haber tener hacer decir ver ir dar era eran fue fueron
         hay habia esta estan estaba tiene tienen tenia hace hacen puede pueden vamos vale claro bueno
         entonces pues asi tambien siempre nunca ahora luego yo tu el ella nosotros ellos me te se nos
         le les lo mi su sus al del no si ya en es son hasta desde cosa cosas gente vez veces
         the a of to in on for and or but is are was were be have has it that this you i"
            .split_whitespace()
            .collect()
    })
}

type Counter = HashMap<String, usize>;

// ----------------------------------------------------------------------------
// Fronteras de tema
// ----------------------------------------------------------------------------

/// Instantes del podcast en los que cambia el tema de conversacion.
#[derive(Debug, Clone, Default)]
pub struct TopicMap {
    boundaries: Vec<f64>,
}

impl TopicMap {
    pub fn new(mut boundaries: Vec<f64>) -> Self {
        boundaries.sort_by(|a, b| a.total_cmp(b));
        Self { boundaries }
    }

    pub fn len(&self) -> usize {
        self.boundaries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.boundaries.is_empty()
    }

    /// Segundos desde la frontera de tema anterior mas cercana. Se mira solo
    /// hacia atras: empezar 5 s despues de que arranque un tema es correcto;
    /// empezar 5 s ANTES es empezar dentro del tema anterior.
    pub fn distance(&self, moment: f64) -> f64 {
        let mut best = f64::INFINITY;
        for &boundary in &self.boundaries {
            if boundary > moment + 1.0 {
                break;
            }
            best = moment - boundary;
        }
        if best >= 0.0 { best } else { f64::INFINITY }
    }
}

struct Block {
    start: f64,
    words: Vec<String>,
}

/// Detecta cambios de tema por cohesion lexica (TextTiling).
pub fn build_topic_map(transcript: &Transcript, cfg: &ContextConfig) -> TopicMap {
    let blocks = blocks(transcript, cfg.block_seconds);
    let span = cfg.compare_blocks.max(0) as usize;
    if blocks.len() < 2 * span + 2 {
        log::debug!("Transcripcion demasiado corta para detectar temas.");
        return TopicMap::default();
    }

    let count = |blocks: &[Block]| {
        let mut counter = Counter::new();
        for block in blocks {
            for word in &block.words {
                *counter.entry(word.clone()).or_default() += 1;
            }
        }
        counter
    };
    let gaps: Vec<(usize, f64)> =
        (span..blocks.len() - span).map(|i| (i, 1.0 - cosine(&count(&blocks[i - span..i]), &count(&blocks[i..i + span])))).collect();
    if gaps.is_empty() {
        return TopicMap::default();
    }

    // Solo interesan los maximos locales: una zona uniformemente distinta no es
    // una frontera, lo es el punto exacto donde cambia.
    let values: Vec<f64> = gaps.iter().map(|g| g.1).collect();
    let mean = values.iter().sum::<f64>() / values.len() as f64;
    let deviation = (values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / values.len() as f64).sqrt();
    let threshold = mean + cfg.sensitivity * deviation;

    let mut boundaries = Vec::new();
    for (position, &(index, value)) in gaps.iter().enumerate() {
        if value < threshold {
            continue;
        }
        let neighbourhood = &values[position.saturating_sub(2)..(position + 3).min(values.len())];
        if value >= neighbourhood.iter().cloned().fold(f64::NEG_INFINITY, f64::max) {
            boundaries.push(blocks[index].start);
        }
    }

    log::info!("Temas: {} cambios de tema detectados en la transcripcion.", boundaries.len());
    TopicMap::new(boundaries)
}

/// Trocea la transcripcion en bloques de duracion fija.
fn blocks(transcript: &Transcript, seconds: f64) -> Vec<Block> {
    let mut blocks: Vec<Block> = Vec::new();
    let mut current: Vec<String> = Vec::new();
    let mut anchor: Option<f64> = None;

    for word in transcript.iter_words() {
        let at = *anchor.get_or_insert(word.start);
        // Un salto grande en el tiempo significa que hay un hueco sin
        // transcribir (preescaneo): los bloques no pueden cruzarlo.
        if word.start - at > seconds || (!current.is_empty() && word.start - at < 0.0) {
            blocks.push(Block { start: at, words: std::mem::take(&mut current) });
            anchor = Some(word.start);
        }
        let token = fold(&word.text);
        if token.chars().count() >= 3 && !stop_words().contains(token.as_str()) {
            current.push(token);
        }
    }
    if let (false, Some(at)) = (current.is_empty(), anchor) {
        blocks.push(Block { start: at, words: current });
    }
    blocks.retain(|b| !b.words.is_empty());
    blocks
}

fn cosine(a: &Counter, b: &Counter) -> f64 {
    let dot: f64 = a.iter().filter_map(|(word, &x)| b.get(word).map(|&y| (x * y) as f64)).sum();
    if dot == 0.0 {
        return 0.0;
    }
    let norm = |m: &Counter| m.values().map(|&v| (v * v) as f64).sum::<f64>().sqrt();
    let (na, nb) = (norm(a), norm(b));
    if na > 0.0 && nb > 0.0 { dot / (na * nb) } else { 0.0 }
}

// ----------------------------------------------------------------------------
// Puntuacion del arranque de una ventana
// ----------------------------------------------------------------------------

/// Rellena `window.context` (0..1) en cada ventana.
pub fn apply(windows: &mut [Window], topics: &TopicMap, cfg: &ContextConfig) {
    for window in windows {
        window.context = score(window, topics, cfg);
    }
}

fn score(window: &Window, topics: &TopicMap, cfg: &ContextConfig) -> f64 {
    let opening = first_sentence(window);
    if opening.is_empty() {
        return 0.0;
    }

    let mut score = 0.5; // punto de partida neutro

    // 0. La senal mas barata y mas fiable: Whisper pone mayuscula al empezar
    //    frase. Si la primera palabra va en minuscula, el clip arranca
    //    literalmente a mitad de una frase. Nada lo arregla despues.
    if starts_mid_sentence(window) {
        score -= 0.45;
    }

    // 1. Empezar donde empieza un tema.
    let distance = topics.distance(window.start);
    if distance <= cfg.boundary_grace {
        score += 0.35;
    } else if distance <= cfg.boundary_grace * 3.0 {
        score += 0.12;
    }

    // 2. Arranque anaforico: el espectador no ha visto aquello a lo que se
    //    refiere el que habla.
    let lowered = fold(&opening);
    if ANAPHORIC.iter().any(|p| lowered.starts_with(p)) {
        score -= 0.40;
    } else if OPENING.iter().any(|p| lowered.starts_with(p)) || opening.trim_start().starts_with('¿') {
        score += 0.20;
    }

    // 3. El asunto del clip debe nombrarse pronto. Si las palabras que mas se
    //    repiten en todo el clip no aparecen hasta la mitad, el arranque no
    //    dice de que se esta hablando.
    score += 0.25 * subject_introduced(window, cfg);

    score.clamp(0.0, 1.0)
}

fn starts_mid_sentence(window: &Window) -> bool {
    let Some(first) = window.words.first().map(|w| w.text.trim()) else { return false };
    match first.chars().next() {
        // Un nombre propio tambien va en mayuscula, pero eso solo produce
        // falsos negativos (no marcar un corte malo), nunca falsos positivos.
        Some(c) if c.is_alphabetic() => c.is_lowercase(),
        _ => false,
    }
}

fn first_sentence(window: &Window) -> String {
    if window.words.is_empty() {
        return window.text.chars().take(120).collect();
    }
    let mut parts: Vec<&str> = Vec::new();
    for word in &window.words {
        parts.push(word.text.trim());
        if word.ends_sentence() && parts.len() >= 3 {
            break;
        }
    }
    parts.join(" ")
}

/// Fraccion de los temas del clip que ya se nombran en su arranque.
fn subject_introduced(window: &Window, cfg: &ContextConfig) -> f64 {
    if window.words.is_empty() {
        return 0.0;
    }
    let content: Vec<(f64, String)> = window
        .words
        .iter()
        .map(|w| (w.start, fold(&w.text)))
        .filter(|(_, token)| token.chars().count() >= 4 && !stop_words().contains(token.as_str()))
        .collect();
    if content.len() < 6 {
        return 0.0;
    }

    // Los cuatro mas repetidos; en empate manda quien aparecio antes
    // (como `Counter.most_common`).
    let mut order: Vec<&str> = Vec::new();
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for (_, token) in &content {
        if !counts.contains_key(token.as_str()) {
            order.push(token);
        }
        *counts.entry(token).or_default() += 1;
    }
    order.sort_by_key(|token| std::cmp::Reverse(counts[token]));
    let top = &order[..order.len().min(4)];
    if top.is_empty() {
        return 0.0;
    }

    let cutoff = window.start + cfg.intro_seconds;
    let early: HashSet<&str> = content.iter().filter(|(t, _)| *t <= cutoff).map(|(_, tok)| tok.as_str()).collect();
    top.iter().filter(|word| early.contains(*word)).count() as f64 / top.len() as f64
}

/// Minusculas y sin tildes (ni ninguna otra marca combinante).
fn fold(text: &str) -> String {
    text.trim().to_lowercase().nfd().filter(|c| !is_combining_mark(*c)).collect()
}

fn is_combining_mark(c: char) -> bool {
    matches!(c, '\u{0300}'..='\u{036F}' | '\u{1AB0}'..='\u{1AFF}' | '\u{1DC0}'..='\u{1DFF}' | '\u{20D0}'..='\u{20FF}' | '\u{FE20}'..='\u{FE2F}')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::words::{Segment, Word};

    fn ventana(inicio: f64, texto: &str) -> Window {
        let palabras: Vec<Word> =
            texto.split_whitespace().enumerate().map(|(i, t)| Word::new(t, inicio + i as f64 * 0.4, inicio + i as f64 * 0.4 + 0.3)).collect();
        Window::new(inicio, inicio + 30.0, palabras, texto)
    }

    fn cfg() -> ContextConfig {
        ContextConfig::default()
    }

    #[test]
    fn fold_quita_tildes_y_mayusculas() {
        assert_eq!(fold("  ¿Qué PASÓ? "), "¿que paso?");
        assert_eq!(fold("Ñandú"), "nandu");
        assert_eq!(fold("canción"), "cancion");
    }

    #[test]
    fn empezar_a_mitad_de_frase_baja_la_nota() {
        let sin_topics = TopicMap::default();
        let bien = score(&ventana(0.0, "Hablamos hoy de bitcoin y de dinero en el banco."), &sin_topics, &cfg());
        let mal = score(&ventana(0.0, "hablamos hoy de bitcoin y de dinero en el banco."), &sin_topics, &cfg());
        assert!(mal < bien, "{mal} >= {bien}");
    }

    #[test]
    fn el_arranque_anaforico_resta_y_la_pregunta_suma() {
        let t = TopicMap::default();
        let anaforico = score(&ventana(0.0, "Entonces eso es lo que le pasó a él."), &t, &cfg());
        let neutro = score(&ventana(0.0, "Los gatos duermen muchas horas al día."), &t, &cfg());
        let pregunta = score(&ventana(0.0, "¿Sabes por qué los gatos duermen tanto?"), &t, &cfg());
        assert!(anaforico < neutro, "{anaforico} >= {neutro}");
        assert!(pregunta > neutro, "{pregunta} <= {neutro}");
    }

    #[test]
    fn empezar_en_una_frontera_de_tema_suma() {
        let ventana = ventana(100.0, "Los gatos duermen muchas horas al día.");
        let en_frontera = score(&ventana, &TopicMap::new(vec![98.0]), &cfg());
        let a_media_charla = score(&ventana, &TopicMap::new(vec![10.0]), &cfg());
        let sin_temas = score(&ventana, &TopicMap::default(), &cfg());
        assert!(en_frontera > a_media_charla && en_frontera > sin_temas);
    }

    #[test]
    fn la_distancia_solo_mira_hacia_atras() {
        let t = TopicMap::new(vec![50.0, 100.0]);
        assert_eq!(t.distance(105.0), 5.0);
        assert_eq!(t.distance(100.5), 0.5 + 0.0);
        // La frontera de 100 queda 0.5 s por delante: cuenta (margen de 1 s), pero
        // entonces el clip empezaria ANTES del tema y no se da por buena.
        assert!(t.distance(99.5).is_infinite());
        assert_eq!(t.distance(60.0), 10.0);
        assert!(t.distance(10.0).is_infinite());
        assert!(TopicMap::default().distance(10.0).is_infinite());
    }

    #[test]
    fn la_nota_siempre_queda_entre_0_y_1() {
        let t = TopicMap::new(vec![0.0]);
        for texto in ["entonces eso", "¿Qué es esto?", "a b c", "Un texto normal de siempre, nada raro aquí."] {
            let s = score(&ventana(0.0, texto), &t, &cfg());
            assert!((0.0..=1.0).contains(&s), "{texto}: {s}");
        }
        assert_eq!(score(&Window::new(0.0, 30.0, vec![], ""), &t, &cfg()), 0.0);
    }

    #[test]
    fn el_asunto_se_nombra_pronto_o_tarde() {
        let pronto = ventana(0.0, "Bitcoin bitcoin dinero banco inversion hoy. Luego seguimos con bitcoin y mas dinero banco.");
        let tarde: String = format!("{} bitcoin bitcoin dinero banco inversion bitcoin dinero banco", "relleno ".repeat(50));
        let tarde = ventana(0.0, &tarde);
        assert!(subject_introduced(&pronto, &cfg()) > subject_introduced(&tarde, &cfg()));
        assert_eq!(subject_introduced(&ventana(0.0, "poco texto"), &cfg()), 0.0);
    }

    /// Dos temas muy distintos, 60 s cada uno, uno detras de otro.
    fn dos_temas() -> Transcript {
        let mut palabras = Vec::new();
        for i in 0..150 {
            let a = ["bitcoin", "dinero", "banco", "inversion", "mercado"][i % 5];
            palabras.push(Word::new(a, i as f64 * 0.4, i as f64 * 0.4 + 0.3));
        }
        for i in 0..150 {
            let b = ["perros", "gatos", "veterinario", "vacunas", "paseo"][i % 5];
            let t = 60.0 + i as f64 * 0.4;
            palabras.push(Word::new(b, t, t + 0.3));
        }
        Transcript { language: "es".into(), duration: 120.0, segments: vec![Segment { start: 0.0, end: 120.0, text: String::new(), words: palabras }] }
    }

    #[test]
    fn detecta_el_cambio_de_tema() {
        let mapa = build_topic_map(&dos_temas(), &ContextConfig { compare_blocks: 3, ..Default::default() });
        assert!(!mapa.is_empty());
        let cerca = mapa.boundaries.iter().any(|&b| (b - 60.0).abs() <= 13.0);
        assert!(cerca, "ninguna frontera cerca del segundo 60: {:?}", mapa.boundaries);
    }

    #[test]
    fn una_transcripcion_corta_no_tiene_temas() {
        let corta = Transcript { language: "es".into(), duration: 5.0, segments: vec![] };
        assert!(build_topic_map(&corta, &cfg()).is_empty());
    }

    #[test]
    fn los_bloques_no_cruzan_un_hueco_sin_transcribir() {
        let mut t = dos_temas();
        t.segments[0].words.truncate(60);
        for w in t.segments[0].words.iter_mut() {
            w.start += 0.0;
        }
        let mut lejos = Word::new("perros", 900.0, 900.3);
        lejos.probability = 1.0;
        t.segments[0].words.push(lejos);
        let bloques = blocks(&t, 12.0);
        assert!(bloques.windows(2).all(|p| p[0].start < p[1].start));
        assert!(bloques.last().unwrap().start >= 900.0 - 1e-9);
    }
}
