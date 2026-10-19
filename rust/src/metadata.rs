//! Titulo, gancho, descripcion y hashtags SIN modelo de lenguaje.
//!
//! Claude redacta. Esto no: esto *extrae*. Elige la mejor frase que ya ha dicho
//! el invitado y la usa tal cual, porque una frase real del clip siempre es
//! preferible a una frase inventada por una plantilla.
//!
//! La consecuencia es honesta: el gancho sera bueno cuando el invitado empiece
//! el clip con una frase potente, y mediocre cuando empiece con un "bueno,
//! pues...". Un modelo reescribiria eso; aqui solo se puede elegir la menos mala
//! de las frases disponibles.
//!
//! Se usa cuando el LLM esta desactivado, y tambien para rellenar los huecos que
//! deje el LLM si responde a medias.

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use regex::Regex;
use unicode_normalization::UnicodeNormalization;

use crate::config::Config;
use crate::llm::ClipMetadata;
use crate::scoring::Window;

// ----------------------------------------------------------------------------
// Vocabulario
// ----------------------------------------------------------------------------

/// Palabras que no dicen nada del tema y no pueden acabar en un hashtag.
const STOPWORDS: &str = "
que de la el los las un una unos unas y o pero porque como para por con sin sobre entre
cuando donde quien cual esto eso aquello este esta estos estas ese esa esos esas aqui alli ahi
muy mas menos tan tanto poco mucho todo toda todos todas nada algo alguien nadie cada otro otra
mismo misma ser estar haber tener hacer decir ver ir dar saber poder querer sabes digo dice dicen
era eran fue fueron hay habia esta estan estaba tiene tienen tenia hace hacen hacia puede pueden
vamos vale claro bueno entonces osea pues asi tambien siempre nunca ahora luego despues antes aun
yo tu el ella nosotros vosotros ellos ellas me te se nos os le les lo mi su sus tus mis
al del no si ya en es son soy eres hasta desde cosa cosas gente manera forma parte vez
veces tema punto verdad momento dos tres anos ano
hablo hablas habla hablan hablar hago haces hacemos doy das damos tengo tienes tenemos voy vas vamos
creo crees cree creen quiero quieres quiere queremos puedo puedes podemos digo dices decimos veo ves
vemos gusta gustan gustaba sabes sabe sabemos parece parecen pasa pasan paso pone ponen queda quedan
mira miras oye venga anda salir sale salen llega llegan sigue siguen sido estado hecho dicho visto
want wants think thinks make makes goes going
the a an and or but because as of to in on for with without about that this these those
is are was were be been have has had do does did will would can could should you i he she
it we they them his her its our their not just like know think really very there here what
when where who how why thing things people
";

/// Empezar por una de estas es empezar a media conversacion: mal gancho.
const FILLER_START: &[&str] = &[
    "bueno", "o sea", "osea", "entonces", "pues", "y ", "pero ", "porque ", "eh ", "em ", "claro", "es decir",
    "vale", "digamos", "total", "en fin", "asi que", "así que", "de hecho", "por eso", "sea", "que ", "no ", "si ",
    "well ", "so ", "and ", "but ", "because ", "like ", "i mean", "yeah",
];

/// Empezar por una de estas suele parar el scroll.
const STRONG_START: &[&str] = &[
    "nunca", "jamas", "jamás", "nadie", "todo el mundo", "la verdad", "el problema", "resulta que", "imagina",
    "piensa", "escucha", "mira", "fijate", "fíjate", "lo que", "hay una", "hay un", "existe", "en realidad",
    "de verdad", "te juro", "te lo juro", "os juro", "os lo juro", "no te", "no me", "no hay", "nobody", "never",
    "everyone", "imagine", "the problem", "the truth", "listen", "look",
];

/// Absolutos y superlativos: dan tension a una frase.
const INTENSE: &[&str] = &[
    "nunca", "jamas", "jamás", "siempre", "nadie", "todo el mundo", "el mejor", "la mejor", "el peor", "la peor",
    "el unico", "el único", "imposible", "increible", "increíble", "brutal", "terrible", "flipante", "loco",
    "never", "always", "nobody", "everyone", "best", "worst", "only", "impossible", "incredible", "insane", "crazy",
];

/// Cortar dejando una de estas al final deja la frase prometiendo algo que no
/// llega: se quitan al truncar.
const DANGLING: &str = "
el la los las un una unos unas del al de y o que en con por para sin sobre su sus mi tu lo se es muy mas tan
como pero porque the a an of to in on for and or with my his her its very so but because that this
";

/// Arranques en primera persona: la frase va sobre quien habla, no sobre el tema.
const SELF_REFERENTIAL: &[&str] = &[
    "hago", "hablo", "doy", "tengo", "voy", "quiero", "intento", "no voy", "yo hablo", "yo hago", "a mi me",
    "me da", "me gusta", "no me", "yo no", "cuidado, yo", "no, te doy", "aqui nadie", "i do", "i make", "i want",
];

/// Terminaciones de conjugacion en castellano. "#estaban" o "#aceptan" no los
/// busca nadie; "#hispanidad" o "#guillotina" si.
const VERB_ENDINGS: &[&str] = &[
    "aban", "ieron", "aron", "aran", "eran", "iran", "emos", "amos", "imos", "ando", "endo", "aria", "eria", "iria",
    "abas", "istes", "aste", "iste", "aran", "asen", "esen", "iesen", "aria", "ando", "arse", "erse", "irse",
    "arlo", "erlo", "irlo", "arte", "amos",
];

fn stopwords() -> &'static HashSet<&'static str> {
    static SET: OnceLock<HashSet<&'static str>> = OnceLock::new();
    SET.get_or_init(|| STOPWORDS.split_whitespace().collect())
}

fn dangling() -> &'static HashSet<&'static str> {
    static SET: OnceLock<HashSet<&'static str>> = OnceLock::new();
    SET.get_or_init(|| DANGLING.split_whitespace().collect())
}

/// Las listas de arranques, ya sin tildes ni mayusculas.
fn folded(list: &'static [&'static str]) -> Vec<String> {
    list.iter().map(|s| fold(s)).collect()
}

fn filler_start() -> &'static Vec<String> {
    static V: OnceLock<Vec<String>> = OnceLock::new();
    V.get_or_init(|| folded(FILLER_START))
}

fn strong_start() -> &'static Vec<String> {
    static V: OnceLock<Vec<String>> = OnceLock::new();
    V.get_or_init(|| folded(STRONG_START))
}

fn intense() -> &'static Vec<String> {
    static V: OnceLock<Vec<String>> = OnceLock::new();
    V.get_or_init(|| folded(INTENSE))
}

fn self_referential() -> &'static Vec<String> {
    static V: OnceLock<Vec<String>> = OnceLock::new();
    V.get_or_init(|| folded(SELF_REFERENTIAL))
}

fn regex(pattern: &'static str, cell: &'static OnceLock<Regex>) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).expect("expresion regular valida"))
}

// ----------------------------------------------------------------------------
// API
// ----------------------------------------------------------------------------

/// Metadatos de cada clip. Rellena solo lo que falte en `existing`.
pub fn write_metadata(
    clips: &[Window],
    video_title: &str,
    cfg: &Config,
    existing: Option<&HashMap<usize, ClipMetadata>>,
) -> HashMap<usize, ClipMetadata> {
    if clips.is_empty() {
        return HashMap::new();
    }

    let mut out: HashMap<usize, ClipMetadata> = existing.cloned().unwrap_or_default();
    let source_tags = tags_from_title(video_title);
    let show = show_name(video_title);

    for (index, clip) in clips.iter().enumerate() {
        if out.get(&index).is_some_and(is_complete) {
            continue;
        }
        let built = for_clip(clip, &source_tags, cfg, &show);
        let current = out.get(&index);
        // Si Claude escribio parte, se respeta lo suyo y solo se rellenan huecos.
        let pick = |own: Option<&String>, fallback: String| own.filter(|s| !s.is_empty()).cloned().unwrap_or(fallback);
        let hashtags = current.map(|c| c.hashtags.clone()).filter(|t| !t.is_empty()).unwrap_or(built.hashtags);
        out.insert(
            index,
            ClipMetadata {
                title: pick(current.map(|c| &c.title), built.title),
                hook: pick(current.map(|c| &c.hook), built.hook),
                description: pick(current.map(|c| &c.description), built.description),
                hashtags,
            },
        );
    }
    out
}

/// El gancho no entra: puede estar vacio a proposito (ver `for_clip`).
fn is_complete(meta: &ClipMetadata) -> bool {
    !meta.title.is_empty() && !meta.description.is_empty() && !meta.hashtags.is_empty()
}

// ----------------------------------------------------------------------------
// Construccion de un clip
// ----------------------------------------------------------------------------

fn for_clip(clip: &Window, source_tags: &[String], cfg: &Config, show: &str) -> ClipMetadata {
    let sentences = sentences_of(clip);
    if sentences.is_empty() {
        return ClipMetadata { title: clip_fallback(clip, cfg), hook: String::new(), description: String::new(), hashtags: source_tags.to_vec() };
    }
    let meta = &cfg.metadata;

    // El gancho sale de las primeras frases: es el rotulo de los primeros
    // segundos, y prometer algo que se dice al final seria mentir.
    let mut opening: Vec<usize> = (0..sentences.len()).filter(|&i| sentences[i].start <= meta.hook_window).collect();
    if opening.is_empty() {
        opening = (0..sentences.len().min(2)).collect();
    }
    let hook_index = first_max(&opening, |&i| hook_score(&sentences[i].text, meta.max_hook)).expect("opening nunca esta vacio");
    let hook_sentence = &sentences[hook_index];

    // Un rotulo malo es peor que ninguno: ocupa media pantalla para no decir
    // nada. Si la mejor frase disponible no llega al minimo, no se pone gancho y
    // la tarjeta de titulo se queda sola haciendo su trabajo.
    let hook = if hook_score(&hook_sentence.text, meta.max_hook) < meta.min_hook_score {
        String::new()
    } else {
        shorten(&hook_sentence.text, meta.max_hook).to_uppercase()
    };

    // El titulo se usa como tarjeta de contexto al arrancar el video, asi que
    // tiene que decir DE QUE va el clip: se premia la frase que nombra los
    // asuntos de los que mas se habla.
    let subjects = key_terms(&clip.text, 4);
    let mut title_pool: Vec<usize> = (0..sentences.len()).filter(|&i| i != hook_index).collect();
    if title_pool.is_empty() {
        title_pool = (0..sentences.len()).collect();
    }
    let title_index = first_max(&title_pool, |&i| title_score(&sentences[i].text, &subjects, meta.max_title)).expect("title_pool nunca esta vacio");
    let title_sentence = &sentences[title_index];

    // Si ninguna frase del clip funciona como titulo, se usa la identidad del
    // programa: informacion verdadera que se ve intencionada.
    let best = title_score(&title_sentence.text, &subjects, meta.max_title);
    let title = if best >= meta.min_title_score {
        shorten(&title_sentence.text, meta.max_title)
    } else if !show.is_empty() {
        show.to_string()
    } else {
        shorten(&title_sentence.text, meta.max_title)
    };

    let rest: Vec<&Sentence> = sentences.iter().enumerate().filter(|(i, _)| *i != hook_index && *i != title_index).map(|(_, s)| s).collect();
    let pool: Vec<&Sentence> = if rest.is_empty() { sentences.iter().collect() } else { rest };
    let description = description(&pool, meta.max_description);

    ClipMetadata { title, hook, description, hashtags: hashtags(&clip.text, source_tags, meta.hashtags.max(0) as usize) }
}

/// Primer maximo (como `max()` de Python: en un empate gana el primero).
fn first_max<T>(items: &[T], key: impl Fn(&T) -> f64) -> Option<T>
where
    T: Copy,
{
    let mut best: Option<(f64, T)> = None;
    for &item in items {
        let score = key(&item);
        if best.is_none_or(|(b, _)| score > b) {
            best = Some((score, item));
        }
    }
    best.map(|(_, item)| item)
}

struct Sentence {
    text: String,
    start: f64,
}

/// Frases del clip con el segundo en el que empieza cada una. Se reconstruyen
/// desde las palabras con marca de tiempo para saber cuales caen dentro de los
/// primeros segundos.
fn sentences_of(clip: &Window) -> Vec<Sentence> {
    if clip.words.is_empty() {
        return split_sentences(&clip.text)
            .into_iter()
            .map(|t| t.trim().to_string())
            .filter(|t| t.chars().count() > 2)
            .map(|text| Sentence { text, start: 0.0 })
            .collect();
    }

    let base = clip.start;
    let mut sentences = Vec::new();
    let mut buffer: Vec<&str> = Vec::new();
    let mut started = clip.words[0].start - base;

    for word in &clip.words {
        if buffer.is_empty() {
            started = word.start - base;
        }
        buffer.push(word.text.trim());
        if word.ends_sentence() {
            let text = buffer.join(" ").trim().to_string();
            if text.chars().count() > 2 {
                sentences.push(Sentence { text, start: started });
            }
            buffer.clear();
        }
    }
    if !buffer.is_empty() {
        let text = buffer.join(" ").trim().to_string();
        if text.chars().count() > 2 {
            sentences.push(Sentence { text, start: started });
        }
    }
    sentences
}

/// Parte un texto en frases: despues de `.`, `!`, `?` o `…` seguidos de blanco.
fn split_sentences(text: &str) -> Vec<String> {
    let mut pieces = Vec::new();
    let mut current = String::new();
    let mut previous: Option<char> = None;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_whitespace() && previous.is_some_and(|p| ".!?…".contains(p)) {
            while chars.peek().is_some_and(|n| n.is_whitespace()) {
                chars.next();
            }
            pieces.push(std::mem::take(&mut current));
            previous = None;
            continue;
        }
        current.push(c);
        previous = Some(c);
    }
    pieces.push(current);
    pieces
}

// ----------------------------------------------------------------------------
// Puntuacion de frases
// ----------------------------------------------------------------------------

fn hook_score(text: &str, max_chars: i64) -> f64 {
    let lowered = fold(text);
    let mut score = 0.0;
    let length = text.chars().count();

    // "Se pudo conquistar" no dice nada aunque no tenga muletillas: sin
    // sustantivos concretos no hay curiosidad que despertar. Las cifras cuentan
    // como contenido: "190 y 128 kilos" es concretisimo con una sola palabra.
    let meaningful = substance(text);

    // Una frase que no cabe en el rotulo hay que cortarla, y un gancho cortado
    // no promete nada. El castigo es alto a proposito: casi cualquier frase
    // entera es mejor que una a medias.
    if length as i64 > max_chars {
        score -= 3.2;
    }

    if meaningful < 2 {
        score -= 2.4;
    } else if meaningful >= 4 {
        score += 0.6;
    }

    // Interrogacion, pero solo si hay algo que preguntar. "¿Eh?" tambien acaba
    // en interrogante y no es un gancho.
    if (text.trim_end().ends_with('?') || text.trim_start().starts_with('¿')) && meaningful >= 3 {
        score += 2.2;
    }
    if has_digit(text) {
        score += 1.4;
    }
    if strong_start().iter().any(|s| lowered.starts_with(s.as_str())) {
        score += 1.6;
    }
    if filler_start().iter().any(|s| lowered.starts_with(s.as_str())) {
        score -= 2.6;
    }
    if intense().iter().any(|w| lowered.contains(w.as_str())) {
        score += 0.9;
    }

    if (14..=65).contains(&length) {
        score += 1.1;
    } else if length < 12 {
        score -= 2.0;
    } else if length > 110 {
        score -= 1.3;
    }

    // Una frase con sustantivos concretos promete algo; una de puro relleno no.
    score + (content_ratio(text) * 2.0).min(1.4)
}

/// Como de bueno es este texto como tarjeta de contexto.
///
/// Lo que se busca NO es la frase mejor construida, es la que dice de que va el
/// clip: manda la cobertura del tema por encima de todo lo demas.
fn title_score(text: &str, subjects: &[String], max_chars: i64) -> f64 {
    let mut score = 0.0;
    let folded_text = fold(text);
    let length = text.chars().count();

    if !subjects.is_empty() {
        let hits = subjects.iter().filter(|term| folded_text.contains(term.as_str())).count();
        score += 3.5 * (hits as f64 / subjects.len() as f64);
    }

    // Informacion en terminos absolutos, no en proporcion: medir la FRACCION de
    // palabras con significado premia a la frase corta y vacia.
    score += substance(text).min(7) as f64 * 0.35;

    if has_digit(text) {
        score += 0.8;
    }
    if text.trim_end().ends_with('?') {
        score += 0.9;
    }
    if intense().iter().any(|w| folded_text.contains(w.as_str())) {
        score += 0.6;
    }
    if filler_start().iter().any(|s| folded_text.starts_with(s.as_str())) {
        score -= 1.6;
    }

    // Una tarjeta de contexto habla del TEMA, no de las costumbres del que
    // habla.
    if self_referential().iter().any(|s| folded_text.starts_with(s.as_str())) {
        score -= 1.5;
    }

    if length as i64 > max_chars {
        score -= 2.0; // habria que cortarla, y una tarjeta a medias no vale
    } else if (26..=max_chars).contains(&(length as i64)) {
        score += 1.0;
    } else if length < 18 {
        score -= 1.8;
    }
    score
}

/// Unidades de significado de la frase: palabras plenas y cifras.
fn substance(text: &str) -> usize {
    let words = letter_runs(text).iter().filter(|w| !stopwords().contains(fold(w).as_str())).count();
    words + digit_runs(text)
}

/// Fraccion de palabras que aportan significado (no muletillas).
fn content_ratio(text: &str) -> f64 {
    let words = letter_runs(text);
    if words.is_empty() {
        return 0.0;
    }
    let meaningful = words.iter().filter(|w| !stopwords().contains(fold(w).as_str())).count();
    meaningful as f64 / words.len() as f64
}

// ----------------------------------------------------------------------------
// Hashtags
// ----------------------------------------------------------------------------

/// Palabras con peso, en orden de aparicion, con su numero de repeticiones.
fn weighted_terms(text: &str) -> Vec<(String, usize)> {
    let mut order: Vec<(String, usize)> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    for raw in letter_runs(text) {
        let word = fold(&raw);
        if stopwords().contains(word.as_str()) || word.chars().count() < 4 || looks_like_verb(&word) {
            continue;
        }
        match index.get(&word) {
            Some(&i) => order[i].1 += 1,
            None => {
                index.insert(word.clone(), order.len());
                order.push((word, 1));
            }
        }
    }
    // A igualdad de repeticiones gana la palabra mas larga: es la mas
    // especifica. En un empate total, el orden de aparicion.
    order.sort_by(|a, b| b.1.cmp(&a.1).then(b.0.chars().count().cmp(&a.0.chars().count())));
    order
}

/// Las palabras de las que mas se habla en el clip: su asunto.
fn key_terms(text: &str, limit: usize) -> Vec<String> {
    weighted_terms(text).into_iter().take(limit).map(|(word, _)| word).collect()
}

/// Las palabras con mas peso del clip, mas las del podcast de origen.
fn hashtags(text: &str, source_tags: &[String], limit: usize) -> Vec<String> {
    let ranked = weighted_terms(text);
    let mut tags: Vec<String> = Vec::new();
    for tag in source_tags.iter().cloned().chain(ranked.into_iter().map(|(word, _)| word)) {
        if !tags.contains(&tag) {
            tags.push(tag);
        }
        if tags.len() >= limit {
            break;
        }
    }
    for generic in ["podcast", "shorts"] {
        if tags.len() < limit && !tags.iter().any(|t| t == generic) {
            tags.push(generic.to_string());
        }
    }
    tags.truncate(limit);
    tags
}

fn looks_like_verb(word: &str) -> bool {
    word.chars().count() > 5 && VERB_ENDINGS.iter().any(|ending| word.ends_with(ending))
}

/// Texto de la tarjeta de contexto, segun `title_card.source`.
///
/// Por defecto es el titulo del video de origen sin la lista de temas del
/// episodio: "The Wild Project #375 - Xokas | ¿Por que no pelea..." se queda en
/// "The Wild Project - Xokas". Es informacion verdadera, siempre esta bien
/// escrita, y dice al que llega de golpe QUE esta viendo.
pub fn card_text(video_title: &str, clip_title: &str, cfg: &Config) -> String {
    let non_empty = |text: &str, fallback: &str| if text.is_empty() { fallback.to_string() } else { text.to_string() };
    match cfg.title_card.source.as_str() {
        "video" => non_empty(video_title, clip_title),
        "clip" => clip_title.to_string(),
        _ => {
            let show = show_name(video_title);
            if !show.is_empty() { show } else { non_empty(video_title, clip_title) }
        }
    }
}

/// Programa e invitado, sacados del titulo del video de origen.
/// "The Wild Project #375 - Xokas | ¿Por que no pelea..." -> "The Wild Project\nXokas".
/// Es el respaldo del titulo: nunca queda raro y siempre es cierto.
pub fn show_name(video_title: &str) -> String {
    if video_title.is_empty() {
        return String::new();
    }
    static EPISODE: OnceLock<Regex> = OnceLock::new();
    static DASH: OnceLock<Regex> = OnceLock::new();
    let head = video_title.split('|').next().unwrap_or("").trim();
    let head = regex(r"#\s*\d+", &EPISODE).replace_all(head, "");
    let head = head.trim_matches(|c: char| " -–—·|".contains(c));
    let parts: Vec<&str> = regex(r"\s[-–—]\s", &DASH).split(head).map(str::trim).filter(|p| !p.is_empty()).collect();
    match parts.as_slice() {
        [] => String::new(),
        // En dos lineas a proposito: el programa arriba y el invitado debajo se
        // lee mejor que una linea larga partida por donde caiga.
        [first, second, ..] => format!("{first}\n{second}"),
        [only] => only.to_string(),
    }
}

/// Un unico hashtag con el nombre del programa. Trocear "The Wild Project #375
/// - Xokas" en #wild y #project da dos hashtags que no busca nadie;
/// "#thewildproject" si es el canal.
fn tags_from_title(video_title: &str) -> Vec<String> {
    if video_title.is_empty() {
        return Vec::new();
    }
    static SEPARATORS: OnceLock<Regex> = OnceLock::new();
    // El nombre del programa es lo que va antes del numero de episodio o del
    // primer separador.
    let show = regex(r"[|#–—]|\s-\s", &SEPARATORS).split(video_title).next().unwrap_or("");
    let words: Vec<String> = letter_runs(show).iter().map(|w| fold(w)).collect();
    let few = words.len() <= 2;
    let kept: Vec<&String> = words.iter().filter(|w| !stopwords().contains(w.as_str()) || few).collect();
    let slug: String = kept.iter().map(|w| w.as_str()).collect::<String>().chars().take(24).collect();
    if slug.chars().count() >= 5 { vec![slug] } else { Vec::new() }
}

// ----------------------------------------------------------------------------
// Utilidades de texto
// ----------------------------------------------------------------------------

/// Una o dos frases seguidas, sin pasarse de largo.
fn description(sentences: &[&Sentence], max_chars: i64) -> String {
    let mut ranked: Vec<&&Sentence> = sentences.iter().collect();
    ranked.sort_by(|a, b| title_score(&b.text, &[], 70).total_cmp(&title_score(&a.text, &[], 70)));

    let mut out = String::new();
    for sentence in ranked.into_iter().take(2) {
        let candidate = format!("{out} {}", sentence.text).trim().to_string();
        if candidate.chars().count() as i64 > max_chars && !out.is_empty() {
            break;
        }
        out = candidate;
    }
    shorten(&out, max_chars)
}

/// Recorta por frontera de palabra, sin dejar articulos colgando. Cortar en
/// "...se quitaron las" es peor que cortar en "...se quitaron": la palabra
/// huerfana promete un sustantivo que nunca llega.
pub fn shorten(text: &str, limit: i64) -> String {
    let text = tidy(text);
    let limit = limit.max(0) as usize;
    if text.chars().count() <= limit {
        return text;
    }

    let mut cut: Vec<char> = text.chars().take(limit + 1).collect();
    if let Some(space) = cut.iter().rposition(|&c| c == ' ') {
        if space as f64 > limit as f64 * 0.55 {
            cut.truncate(space);
        }
    }

    let joined: String = cut.into_iter().collect();
    let mut words: Vec<&str> = joined.split_whitespace().collect();
    while words.len() > 3 && dangling().contains(fold(words[words.len() - 1].trim_matches(|c: char| ".,;:¿?¡!".contains(c))).as_str()) {
        words.pop();
    }
    format!("{}…", words.join(" ").trim_end_matches(|c: char| " ,;:.-".contains(c)))
}

/// Limpia los tics de la transcripcion: espacios sueltos antes de coma.
pub fn tidy(text: &str) -> String {
    static SPACES: OnceLock<Regex> = OnceLock::new();
    static BEFORE: OnceLock<Regex> = OnceLock::new();
    static AFTER: OnceLock<Regex> = OnceLock::new();
    let text = regex(r"\s+", &SPACES).replace_all(text, " ");
    let text = regex(r"\s+([,.;:!?…])", &BEFORE).replace_all(text.trim(), "$1");
    let text = regex(r"([¿¡])\s+", &AFTER).replace_all(&text, "$1");
    text.trim_matches(|c: char| "-–—,;: ".contains(c)).to_string()
}

fn clip_fallback(clip: &Window, cfg: &Config) -> String {
    let text = if clip.text.is_empty() { "Clip" } else { clip.text.as_str() };
    shorten(text, cfg.metadata.max_title)
}

/// Minusculas sin tildes: 'Increíble' y 'increible' son la misma palabra.
fn fold(text: &str) -> String {
    text.to_lowercase().nfd().filter(|c| !matches!(*c, '\u{0300}'..='\u{036F}' | '\u{1AB0}'..='\u{1AFF}' | '\u{1DC0}'..='\u{1DFF}' | '\u{20D0}'..='\u{20FF}' | '\u{FE20}'..='\u{FE2F}')).collect()
}

/// Tramos de letras de tres o mas caracteres, en minusculas.
fn letter_runs(text: &str) -> Vec<String> {
    let lowered = text.to_lowercase();
    let mut runs = Vec::new();
    let mut current = String::new();
    for c in lowered.chars() {
        if c.is_alphabetic() {
            current.push(c);
        } else {
            if current.chars().count() >= 3 {
                runs.push(std::mem::take(&mut current));
            }
            current.clear();
        }
    }
    if current.chars().count() >= 3 {
        runs.push(current);
    }
    runs
}

fn has_digit(text: &str) -> bool {
    text.chars().any(|c| c.is_numeric())
}

fn digit_runs(text: &str) -> usize {
    let mut count = 0;
    let mut inside = false;
    for c in text.chars() {
        let digit = c.is_numeric();
        if digit && !inside {
            count += 1;
        }
        inside = digit;
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::words::Word;

    /// Un clip con frases de 0.4 s por palabra a partir del segundo `inicio`.
    fn clip(frases: &[&str]) -> Window {
        let mut words = Vec::new();
        let mut t = 100.0;
        for frase in frases {
            let partes: Vec<&str> = frase.split_whitespace().collect();
            for (i, parte) in partes.iter().enumerate() {
                let texto = if i == partes.len() - 1 && !".!?".contains(parte.chars().last().unwrap()) { format!("{parte}.") } else { parte.to_string() };
                words.push(Word::new(texto, t, t + 0.3));
                t += 0.4;
            }
        }
        let text = words.iter().map(|w| w.text.as_str()).collect::<Vec<_>>().join(" ");
        Window::new(100.0, t, words, text)
    }

    fn cfg() -> Config {
        Config::default()
    }

    // -- limpieza ----------------------------------------------------------------

    #[test]
    fn los_espacios_sueltos_se_quitan_sin_comerse_la_puntuacion() {
        assert_eq!(tidy("¿ Qué es esto , de verdad ? Si ."), "¿Qué es esto, de verdad? Si.");
        assert_eq!(tidy("  - hola  mundo ;"), "hola mundo");
    }

    #[test]
    fn el_recorte_no_deja_articulos_colgando() {
        let largo = "A finales de los setenta se quitaron las leyes que protegian a los pequeños comercios del barrio";
        let corto = shorten(largo, 45);
        assert!(corto.ends_with('…') && corto.chars().count() <= 46, "{corto}");
        assert!(!corto.trim_end_matches('…').ends_with(" las") && !corto.trim_end_matches('…').ends_with(" los"), "{corto}");
        assert_eq!(shorten("Corto y bien", 45), "Corto y bien");
        assert_eq!(shorten("¿Por qué los gatos duermen tantas horas al día , de verdad ?", 70), "¿Por qué los gatos duermen tantas horas al día, de verdad?");
    }

    #[test]
    fn una_palabra_enorme_se_corta_donde_cae() {
        let corto = shorten(&"a".repeat(100), 20);
        assert_eq!(corto.chars().count(), 22);
    }

    // -- nombre del programa -----------------------------------------------------------

    #[test]
    fn programa_e_invitado_del_titulo_del_video() {
        assert_eq!(show_name("The Wild Project #375 - Xokas | ¿Por que no pelea mas?"), "The Wild Project\nXokas");
        assert_eq!(show_name("Mi Podcast #12"), "Mi Podcast");
        assert_eq!(show_name("Solo un titulo"), "Solo un titulo");
        assert_eq!(show_name(""), "");
        assert_eq!(show_name(" - | resto"), "");
    }

    #[test]
    fn un_hashtag_con_el_nombre_del_programa() {
        // "the" es una palabra vacia: igual que en la version Python.
        assert_eq!(tags_from_title("The Wild Project #375 - Xokas | tema"), ["wildproject"]);
        assert!(tags_from_title("Hola").is_empty());
        assert!(tags_from_title("").is_empty());
        assert_eq!(tags_from_title("Los Increibles Podcast Del Año Pasado Cosas Raras Variadas")[0].chars().count(), 24);
    }

    #[test]
    fn la_tarjeta_segun_el_origen_del_texto() {
        let mut c = cfg();
        let video = "The Wild Project #375 - Xokas | ¿Por que no pelea mas?";
        assert_eq!(card_text(video, "Una frase del clip.", &c), "The Wild Project\nXokas");
        c.title_card.source = "video".into();
        assert_eq!(card_text(video, "clip", &c), video);
        assert_eq!(card_text("", "clip", &c), "clip");
        c.title_card.source = "clip".into();
        assert_eq!(card_text(video, "Una frase del clip.", &c), "Una frase del clip.");
        c.title_card.source = "show".into();
        assert_eq!(card_text("", "frase", &c), "frase");
    }

    // -- frases ------------------------------------------------------------------------

    #[test]
    fn las_frases_se_parten_despues_de_un_final_con_espacio() {
        assert_eq!(split_sentences("Hola. Qué tal? Bien…  Vale"), ["Hola.", "Qué tal?", "Bien…", "Vale"]);
        assert_eq!(split_sentences("3.5 euros, sin más"), ["3.5 euros, sin más"]);
        assert_eq!(split_sentences("Termina así. "), ["Termina así.", ""]);
    }

    #[test]
    fn las_palabras_y_las_cifras() {
        assert_eq!(letter_runs("¡Hola, 190 kilos de año!"), ["hola", "kilos", "año"]);
        assert_eq!(digit_runs("190 y 128 kilos, 7"), 3);
        assert!(substance("190 y 128 kilos") >= 3);
        assert_eq!(fold("Increíble ÑANDÚ"), "increible nandu");
    }

    // -- eleccion ------------------------------------------------------------------------

    #[test]
    fn un_gancho_potente_sale_y_uno_flojo_no() {
        // Resultados de la version Python con exactamente los mismos clips.
        let potente = clip(&["Nunca nadie te ha contado esto del dinero", "Luego seguimos con otras cosas del dia a dia normal"]);
        let meta = &write_metadata(&[potente], "", &cfg(), None)[&0];
        assert_eq!(meta.hook, "NUNCA NADIE TE HA CONTADO ESTO DEL DINERO.");
        assert_eq!(meta.title, "Luego seguimos con otras cosas del dia a dia normal.");
        assert_eq!(meta.hashtags, ["contado", "dinero", "normal", "otras", "podcast", "shorts"]);

        // Una frase que no cabe en el rotulo no se corta: no hay gancho.
        let larga = clip(&["Nunca nadie te ha contado esto sobre el dinero en los bancos", "Luego seguimos hablando de otras cosas normales del dia a dia"]);
        let meta = &write_metadata(&[larga], "El Programa #1 - Invitado", &cfg(), None)[&0];
        assert_eq!(meta.hook, "");
        assert_eq!(meta.title, "Luego seguimos hablando de otras cosas normales del dia a dia.");
        assert_eq!(meta.description, "Luego seguimos hablando de otras cosas normales del dia a dia. Nunca nadie te ha contado esto sobre el dinero en los bancos.");
        assert_eq!(meta.hashtags, ["programa", "normales", "contado", "dinero", "bancos", "otras"]);
    }

    #[test]
    fn el_gancho_sale_de_los_primeros_segundos() {
        // La frase potente empieza a los 11.2 s: aun cuenta (el margen es de 12 s).
        let relleno = "Bueno pues eh vale eh pues bueno entonces eh pues vale bueno eh pues";
        let c = clip(&[relleno, relleno, "Nunca nadie te ha contado esto del dinero"]);
        let meta = &write_metadata(&[c], "", &cfg(), None)[&0];
        assert_eq!(meta.hook, "NUNCA NADIE TE HA CONTADO ESTO DEL DINERO.");
        assert_eq!(meta.title, "Bueno pues eh vale eh pues bueno entonces eh pues vale bueno eh pues.");
    }

    #[test]
    fn un_rotulo_malo_es_peor_que_ninguno_y_el_titulo_cae_al_programa() {
        let flojo = clip(&["Bueno pues eh", "Y entonces eh vale"]);
        let meta = &write_metadata(&[flojo], "El Programa #1 - Invitado", &cfg(), None)[&0];
        assert_eq!(meta.hook, "");
        assert_eq!(meta.title, "El Programa\nInvitado");
        assert_eq!(meta.description, "Y entonces eh vale. Bueno pues eh.");
        assert_eq!(meta.hashtags, ["programa", "podcast", "shorts"]);
    }

    #[test]
    fn una_frase_normal_sirve_de_titulo_aunque_haya_nombre_de_programa() {
        let c = clip(&["Hago mis pinitos y lo que me gusta es aprender", "Voy y vengo"]);
        let meta = &write_metadata(&[c], "The Wild Project #375 - Xokas | tema", &cfg(), None)[&0];
        assert_eq!(meta.title, "Hago mis pinitos y lo que me gusta es aprender.");
        assert_eq!(meta.hashtags, ["wildproject", "aprender", "pinitos", "vengo", "podcast", "shorts"]);
    }

    #[test]
    fn los_hashtags_mezclan_programa_asunto_y_genericos() {
        let c = clip(&["El bitcoin sube porque el bitcoin tiene demanda", "Los bancos compran bitcoin y los inversores tambien"]);
        let meta = write_metadata(&[c], "Mi Programa Favorito #3 - Alguien", &cfg(), None);
        let tags = &meta[&0].hashtags;
        assert_eq!(*tags, ["programafavorito", "bitcoin", "inversores", "demanda", "compran", "bancos"]);
        assert_eq!(meta[&0].title, "Los bancos compran bitcoin y los inversores tambien.");
        assert!(tags.len() <= cfg().metadata.hashtags as usize);
        assert_eq!(tags.iter().collect::<HashSet<_>>().len(), tags.len(), "sin repetidos");
    }

    #[test]
    fn los_genericos_rellenan_si_hay_poco_vocabulario() {
        let c = clip(&["Si no es así"]);
        let meta = write_metadata(&[c], "", &cfg(), None);
        assert_eq!(meta[&0].hashtags, ["podcast", "shorts"]);
    }

    #[test]
    fn se_respeta_lo_que_escribio_claude_y_se_rellenan_los_huecos() {
        let c = clip(&["Nunca nadie te ha contado esto sobre el dinero en los bancos", "Luego seguimos con otras cosas del dia a dia normal"]);
        let mut existente = HashMap::new();
        existente.insert(0, ClipMetadata { title: "Titulo de Claude".into(), hook: String::new(), description: String::new(), hashtags: vec!["suyo".into()] });
        let meta = write_metadata(&[c], "", &cfg(), Some(&existente));
        assert_eq!(meta[&0].title, "Titulo de Claude");
        assert_eq!(meta[&0].hashtags, ["suyo"]);
        assert_eq!(meta[&0].description, "Luego seguimos con otras cosas del dia a dia normal. Nunca nadie te ha contado esto sobre el dinero en los bancos.");
        assert_eq!(meta[&0].hook, "", "esa frase no cabe en el rotulo");
    }

    #[test]
    fn un_clip_completo_no_se_toca() {
        let c = clip(&["Hola mundo entero"]);
        let completo = ClipMetadata::new("T", "", "D", &["a"]);
        let existente = HashMap::from([(0, completo.clone())]);
        assert_eq!(write_metadata(&[c], "x", &cfg(), Some(&existente))[&0], completo);
    }

    #[test]
    fn sin_palabras_se_usa_el_texto_y_sin_texto_clip() {
        let mut c = Window::new(0.0, 30.0, vec![], "Una frase de texto. Y otra que viene despues del punto.");
        let meta = write_metadata(&[c.clone()], "", &cfg(), None);
        assert!(!meta[&0].title.is_empty() && !meta[&0].description.is_empty());
        c.text = String::new();
        let vacio = write_metadata(&[c], "Mi Programa Largo #2", &cfg(), None);
        assert_eq!(vacio[&0].title, "Clip");
        assert_eq!(vacio[&0].hashtags, ["programalargo"]);
        assert!(write_metadata(&[], "x", &cfg(), None).is_empty());
    }

    #[test]
    fn el_titulo_y_el_gancho_respetan_sus_limites() {
        let largo = "Resulta que a finales de los setenta se quitaron las leyes que protegian a los pequeños comercios del barrio y nadie dijo absolutamente nada al respecto";
        let meta = write_metadata(&[clip(&[largo, largo])], "", &cfg(), None);
        assert!(meta[&0].title.chars().count() <= 71, "{}", meta[&0].title);
        assert!(meta[&0].hook.chars().count() <= 46, "{}", meta[&0].hook);
        assert!(meta[&0].description.chars().count() <= 161);
    }

    #[test]
    fn en_un_empate_gana_el_primero_como_en_python() {
        assert_eq!(first_max(&[10usize, 20, 30], |_| 1.0), Some(10));
        assert_eq!(first_max(&[10usize, 20, 30], |&x| if x == 20 || x == 30 { 5.0 } else { 1.0 }), Some(20));
        assert_eq!(first_max::<usize>(&[], |_| 0.0), None);
    }
}
