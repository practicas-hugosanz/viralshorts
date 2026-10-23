//! Generacion del fichero .ASS de subtitulos estilo TikTok / CapCut.
//!
//! Dos diferencias importantes respecto a una implementacion ingenua:
//!
//! 1. NO hay parpadeo. Si cada linea dura exactamente lo que dura su palabra,
//!    en el hueco entre el final de una palabra y el comienzo de la siguiente
//!    (50-300 ms) no se dibuja nada y el subtitulo titila. Aqui cada linea se
//!    extiende hasta el arranque de la siguiente, de modo que el bloque esta
//!    siempre en pantalla y lo unico que cambia es cual es la palabra resaltada.
//!
//! 2. La palabra activa hace "pop": entra un 18% mas grande y vuelve a su
//!    tamano en 120 ms. Es el detalle que separa un subtitulo automatico de uno
//!    editado.

use std::path::Path;

use crate::config::{BrandConfig, HookConfig, SubtitlesConfig, TitleCardConfig};
use crate::words::Word;

/// Si entre dos palabras hay mas silencio que esto, empieza un bloque nuevo:
/// corresponde a una pausa real del que habla.
const GROUP_BREAK_GAP: f64 = 0.7;
/// Cuanto se mantiene en pantalla el ultimo bloque tras la ultima palabra.
const TAIL_HOLD: f64 = 0.40;

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct SubtitleError(pub String);

#[derive(Debug, Clone, Default)]
pub struct Group {
    pub words: Vec<Word>,
}

impl Group {
    pub fn start(&self) -> f64 {
        self.words[0].start
    }

    pub fn end(&self) -> f64 {
        self.words[self.words.len() - 1].end
    }
}

// ----------------------------------------------------------------------------
// Agrupacion
// ----------------------------------------------------------------------------

/// Limpia los tiempos de whisper antes de montar los bloques. Tal cual llegan
/// traen tres defectos que se ven en pantalla:
/// - la misma palabra dos veces en el borde de dos segmentos ("A / A MECISTA");
/// - varias palabras con el mismo instante y sin duracion: nunca se resaltaban
///   y el bloque aparecia con la ultima ya encendida;
/// - una palabra que empieza antes que la anterior.
pub fn tidy_words(words: &[Word]) -> Vec<Word> {
    let mut out: Vec<Word> = Vec::with_capacity(words.len());
    for word in words {
        let mut word = word.clone();
        if let Some(prev) = out.last() {
            if prev.text.trim().to_lowercase() == word.text.trim().to_lowercase() && (word.start - prev.start).abs() < 0.05 {
                continue;
            }
            word.start = word.start.max(prev.start);
        }
        word.end = word.end.max(word.start);
        out.push(word);
    }

    // Las rachas con el mismo arranque se reparten el tiempo que tienen hasta
    // la siguiente palabra (o, al final, hasta donde acaba la racha). Si ahi no
    // cabe (suele pasar cuando el corte de silencios las junta) se toma el
    // hueco de antes, sin pisar la palabra anterior: si no, desaparecian.
    const MIN_WORD: f64 = 0.12;
    let mut i = 0;
    while i < out.len() {
        let mut j = i + 1;
        while j < out.len() && out[j].start - out[i].start < 0.02 {
            j += 1;
        }
        let count = j - i;
        if count > 1 {
            let want = MIN_WORD * count as f64;
            let mut from = out[i].start;
            let until = match out.get(j) {
                Some(next) => next.start,
                None => out[i..j].iter().map(|w| w.end).fold(from + want, f64::max),
            };
            if until - from < want && i > 0 {
                let floor = out[i - 1].end.max(out[i - 1].start + 0.02);
                from = (until - want).max(floor).min(from);
            }
            let step = (until - from) / count as f64;
            if step >= 0.02 {
                for k in 0..count {
                    out[i + k].start = from + step * k as f64;
                    out[i + k].end = if k + 1 < count { from + step * (k + 1) as f64 } else { out[i + k].end.max(out[i + k].start) };
                }
            }
        }
        i = j;
    }
    out
}

/// Parte la lista de palabras en bloques cortos que caben en pantalla.
pub fn group_words(words: &[Word], cfg: &SubtitlesConfig) -> Vec<Group> {
    let mut groups = Vec::new();
    let mut current: Vec<Word> = Vec::new();

    for (index, word) in words.iter().enumerate() {
        current.push(word.clone());
        let is_last = index == words.len() - 1;
        let gap_to_next = if is_last { 0.0 } else { words[index + 1].start - word.end };

        if current.len() as i64 >= cfg.max_words_per_group || word.ends_sentence() || gap_to_next >= GROUP_BREAK_GAP || is_last {
            groups.push(Group { words: std::mem::take(&mut current) });
        }
    }
    if !current.is_empty() {
        groups.push(Group { words: current });
    }
    groups
}

// ----------------------------------------------------------------------------
// Fichero .ASS
// ----------------------------------------------------------------------------

/// Construye el .ASS completo (cabecera + eventos).
///
/// Arriba pueden aparecer dos rotulos, y NUNCA a la vez: primero la tarjeta de
/// titulo (el contexto: de que va esto) y, cuando esa se va, el gancho.
/// Solaparlos taparia media cara del que habla.
#[allow(clippy::too_many_arguments)]
pub fn build_ass(
    words: &[Word],
    width: i64,
    height: i64,
    cfg_subs: &SubtitlesConfig,
    cfg_hook: Option<&HookConfig>,
    hook_text: Option<&str>,
    duration: f64,
    cfg_title: Option<&TitleCardConfig>,
    title_text: Option<&str>,
) -> Result<String, SubtitleError> {
    let words = tidy_words(words);
    let groups = if cfg_subs.enabled && !words.is_empty() { group_words(&words, cfg_subs) } else { Vec::new() };
    let events = subtitle_events(&groups, cfg_subs, width)?;

    let mut card_end = 0.0;
    let mut overlays: Vec<String> = Vec::new();
    if let (Some(cfg), Some(text)) = (cfg_title.filter(|c| c.enabled), title_text.filter(|t| !t.is_empty())) {
        let card = title_event(text, cfg, duration, Some(cfg_subs));
        if !card.is_empty() {
            overlays.extend(card);
            card_end = if duration != 0.0 { cfg.duration.min(duration) } else { cfg.duration };
        }
    }

    if let (Some(cfg), Some(text)) = (cfg_hook.filter(|c| c.enabled), hook_text.filter(|t| !t.is_empty())) {
        overlays.extend(hook_event(text, cfg, duration, card_end, Some(cfg_subs)));
    }

    let mut all = overlays;
    all.extend(events);
    log::info!("Subtitulos: {} bloques, {} lineas de dialogo.", groups.len(), all.len());
    Ok(header(width, height, cfg_subs, cfg_hook, cfg_title)? + &all.join("\n") + "\n")
}

/// Intervalos (segundos del clip montado) de las palabras que la censura tapa,
/// para silenciar la voz ahi y poner un pitido.
pub fn censored_intervals(words: &[crate::words::Word], subs: &SubtitlesConfig) -> Vec<(f64, f64)> {
    words
        .iter()
        .filter(|w| {
            let plain = w.text.trim();
            !plain.is_empty() && censor(plain, &subs.censor_words, subs.censor_profanity, &subs.censor_char) != plain
        })
        // Un poco de margen: whisper marca el inicio de la palabra algo tarde.
        .map(|w| ((w.start - 0.04).max(0.0), w.end + 0.06))
        .collect()
}

/// Pantalla final: el video se oscurece y sale un texto ("Sigueme para mas") y,
/// si hay, el usuario del canal. Dura los ultimos `card.duration` segundos.
pub fn append_end_card(content: &mut String, card: &crate::config::EndCardConfig, handle: &str, width: i64, height: i64, duration: f64, subs: &SubtitlesConfig) {
    let text = card.text.trim();
    if !card.enabled || duration <= 1.0 || (text.is_empty() && handle.trim().is_empty()) {
        return;
    }
    let length = card.duration.clamp(1.0, 6.0).min(duration - 0.5);
    let start = (duration - length).max(0.0);
    let (from, to) = (format_time(start), format_time(duration));
    // Un rectangulo oscuro de lado a lado (dibujo vectorial de ASS).
    content.push_str(&format!(
        "Dialogue: 4,{from},{to},Main,,0,0,0,,{{\\an7\\pos(0,0)\\fad(300,0)\\bord0\\shad0\\1c&H000000&\\1a&H55&\\p1}}m 0 0 l {width} 0 {width} {height} 0 {height}{{\\p0}}\n"
    ));
    let size = ((subs.font_size as f64) * 0.95).round() as i64;
    let mut body = escape_ass_text(&wrap(&text.to_uppercase(), 16));
    let handle = handle.trim();
    if !handle.is_empty() {
        body.push_str(&format!("\\N{{\\fs{}\\c&H0000F2FF&}}{}", (size as f64 * 0.55).round() as i64, escape_ass_text(handle)));
    }
    content.push_str(&format!(
        "Dialogue: 5,{from},{to},Main,,0,0,0,,{{\\an5\\pos({},{})\\fs{size}\\bord6\\shad0\\c&HFFFFFF&\\3c&H000000&\\fad(300,0)\\fscx90\\fscy90\\t(0,350,\\fscx100\\fscy100)}}{body}\n",
        width / 2,
        height / 2
    ));
}

/// .ASS de un solo fotograma para la portada: el gancho grande, arriba.
pub fn cover_ass(text: &str, width: i64, height: i64, subs: &SubtitlesConfig, hook: &HookConfig) -> String {
    let side = (width as f64 * 0.07) as i64;
    let margin = (height as f64 * 0.14) as i64;
    let size = ((subs.font_size as f64) * 1.25).round() as i64;
    let body = escape_ass_text(&wrap(&text.trim().to_uppercase(), 16));
    let color = hex_to_ass_color(&hook.color, 0).unwrap_or_else(|_| "&H00FFFFFF&".into());
    format!(
        "[Script Info]\nScriptType: v4.00+\nWrapStyle: 2\nScaledBorderAndShadow: yes\nPlayResX: {width}\nPlayResY: {height}\n\n[V4+ Styles]\n\
         Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding\n\
         Style: Cover,{},{size},{color},{color},&H00000000&,&H00000000&,-1,0,0,0,100,100,0,0,1,9,3,8,{side},{side},{margin},1\n\n[Events]\n\
         Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n\
         Dialogue: 0,0:00:00.00,0:00:10.00,Cover,,0,0,0,,{body}\n",
        subs.font_name
    )
}

/// Anade el usuario del canal como una linea suelta que dura todo el video.
/// `logo` = (ancho, alto) con que se dibuja el logo, si hay: el usuario se pone
/// justo debajo (o encima, en las esquinas de abajo) de el.
pub fn append_handle(content: &mut String, brand: &BrandConfig, width: i64, height: i64, duration: f64, subs: &SubtitlesConfig, logo: Option<(i64, i64)>) {
    let handle = brand.handle.trim();
    if handle.is_empty() {
        return;
    }
    let margin = crate::ffmpeg::even((width as f64 * 0.04).round() as i64); // el mismo que usa el logo
    let size = ((subs.font_size as f64) * 0.42).round().max(24.0) as i64;
    let gap = if logo.is_some() { 10 } else { 0 };
    let logo_h = logo.map_or(0, |(_, h)| h);
    let (left, top) = (brand.position.ends_with('l'), brand.position.starts_with('t'));
    let x = if left { margin } else { width - margin };
    let y = if top { margin + logo_h + gap } else { height - margin - logo_h - gap };
    // \an7 arriba-izquierda, \an9 arriba-derecha, \an1 abajo-izquierda, \an3 abajo-derecha.
    let anchor = match (top, left) {
        (true, true) => 7,
        (true, false) => 9,
        (false, true) => 1,
        (false, false) => 3,
    };
    let end = if duration > 0.0 { duration } else { 3600.0 };
    content.push_str(&format!(
        "Dialogue: 3,{},{},Main,,0,0,0,,{{\\an{anchor}\\pos({x},{y})\\fs{size}\\bord3\\shad0\\blur0\\c&HFFFFFF&\\3c&H000000&\\alpha&H25&}}{}\n",
        format_time(0.0),
        format_time(end),
        escape_ass_text(handle)
    ));
}

pub fn write_ass(content: &str, path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // libass espera UTF-8; sin BOM para no confundir a versiones antiguas.
    std::fs::write(path, content)
}

fn header(
    width: i64,
    height: i64,
    subs: &SubtitlesConfig,
    hook: Option<&HookConfig>,
    title: Option<&TitleCardConfig>,
) -> Result<String, SubtitleError> {
    let primary = hex_to_ass_color(&subs.color_normal, 0)?;
    let outline = hex_to_ass_color(&subs.color_outline, 0)?;
    let shadow = hex_to_ass_color(&subs.color_shadow, 0)?;
    let margin_v = (height as f64 * subs.vertical_position_ratio) as i64;
    let side = (width as f64 * 0.07) as i64;

    // Alignment 2 = abajo centrado; MarginV se mide desde el borde inferior.
    // Cada plantilla cambia el tipo de borde, su grosor y sus colores.
    let active = hex_to_ass_color(&subs.color_active, 0)?;
    let (border_style, outline_width, shadow_depth, outline_colour, back_colour) = match subs.style.as_str() {
        // Caja translucida detras del texto (el "grosor" es el relleno de la caja).
        "box" => (3, 14, 0, hex_to_ass_color("#000000", 110)?, hex_to_ass_color("#000000", 255)?),
        "minimal" => (1, 2.min(subs.outline_width), 1, outline.clone(), shadow.clone()),
        "neon" => (1, subs.outline_width.max(4), 0, active.clone(), active.clone()),
        _ => (1, subs.outline_width, subs.shadow_depth, outline.clone(), shadow.clone()),
    };
    let mut styles = vec![format!(
        "Style: Main,{},{},{primary},{primary},{outline_colour},{back_colour},-1,0,0,0,100,100,0,0,{border_style},{outline_width},{shadow_depth},2,{side},{side},{margin_v},1",
        subs.font_name, subs.font_size
    )];

    if let Some(hook) = hook {
        let color = hex_to_ass_color(&hook.color, 0)?;
        let hook_outline = hex_to_ass_color(&hook.color_outline, 0)?;
        let hook_margin = (height as f64 * hook.top_margin_ratio) as i64;
        // Alignment 8 = arriba centrado; aqui MarginV se mide desde arriba.
        styles.push(format!(
            "Style: Hook,{},{},{color},{color},{hook_outline},{hook_outline},-1,0,0,0,100,100,0,0,1,{},2,8,{side},{side},{hook_margin},1",
            hook.font_name,
            hook.font_size,
            3.max(subs.outline_width - 1)
        ));
    }

    if let Some(title) = title.filter(|t| t.enabled) {
        let color = hex_to_ass_color(&title.color, 0)?;
        let card_outline = hex_to_ass_color(&title.color_outline, 0)?;
        let font = if title.font_name.is_empty() { &subs.font_name } else { &title.font_name };
        // Alignment 5 = centro exacto del fotograma; con MarginV a 0 el rotulo
        // cae en la costura entre el panel del podcast y el del gameplay, que
        // es el punto donde primero mira el ojo. BorderStyle 1 (contorno, no
        // caja) para que se vea como los subtitulos y no como un cartel.
        // Con una sola imagen a pantalla completa no hay costura: arriba.
        let (alignment, title_margin) = if title.anchor == "top" { (8, (height as f64 * 0.12) as i64) } else { (5, 0) };
        styles.push(format!(
            "Style: Titulo,{font},{},{color},{color},{card_outline},{card_outline},-1,0,0,0,100,100,0,0,1,{},{},{alignment},{side},{side},{title_margin},1",
            title.font_size, title.outline_width, title.shadow_depth
        ));
    }

    Ok(format!(
        "[Script Info]\nTitle: viralshorts\nScriptType: v4.00+\nWrapStyle: 2\nScaledBorderAndShadow: yes\n\
         YCbCr Matrix: TV.709\nPlayResX: {width}\nPlayResY: {height}\n\n[V4+ Styles]\n\
         Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, \
         Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, \
         MarginL, MarginR, MarginV, Encoding\n{}\n\n[Events]\n\
         Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n",
        styles.join("\n")
    ))
}

fn subtitle_events(groups: &[Group], cfg: &SubtitlesConfig, width: i64) -> Result<Vec<String>, SubtitleError> {
    let active = hex_to_ass_color(&cfg.color_active, 0)?;
    let normal = hex_to_ass_color(&cfg.color_normal, 0)?;
    let outline = hex_to_ass_color(&cfg.color_outline, 0)?;
    let ink = hex_to_ass_color("#000000", 0)?;
    let white = hex_to_ass_color("#FFFFFF", 0)?;
    let pop_ms = (cfg.pop_duration * 1000.0) as i64;
    let style = cfg.style.as_str();

    let mut lines = Vec::new();
    for (group_index, group) in groups.iter().enumerate() {
        // Fin del bloque: se enlaza con el siguiente para que no haya un hueco
        // en negro, pero sin arrastrarlo si viene una pausa larga. NUNCA pasa
        // del arranque del siguiente: dos bloques a la vez en pantalla hacen
        // que libass suba uno de los dos y el subtitulo pega un salto.
        let group_end = if let Some(next) = groups.get(group_index + 1) {
            next.start().min(group.end() + TAIL_HOLD)
        } else {
            group.end() + TAIL_HOLD
        };

        let rendered: Vec<String> = group.words.iter().map(|w| render_word(w, cfg)).collect();
        let layout = fit_group(&rendered, cfg, width);

        for (i, word) in group.words.iter().enumerate() {
            let start = if i > 0 { word.start } else { group.start() };
            // Clave contra el parpadeo: la linea vive hasta que arranca la
            // siguiente palabra, no hasta que acaba la actual.
            let end = group.words.get(i + 1).map_or(group_end, |next| next.start);
            if end - start < 0.02 {
                continue;
            }

            let parts: Vec<String> = rendered
                .iter()
                .enumerate()
                .map(|(j, text)| {
                    let hot = j == i;
                    let pop = if hot && style != "minimal" && cfg.pop_scale != 100 && pop_ms > 0 {
                        format!("\\fscx{0}\\fscy{0}\\t(0,{pop_ms},\\fscx100\\fscy100)", cfg.pop_scale)
                    } else {
                        String::new()
                    };
                    let tags = match (style, hot) {
                        // Rotulador: la palabra que suena lleva un bloque de color detras.
                        ("marker", true) => format!("\\c{ink}\\3c{active}\\bord{}{pop}", cfg.outline_width + 9),
                        ("marker", false) => format!("\\c{normal}\\3c{outline}\\bord{}", cfg.outline_width),
                        // Neon: brillo del color de acento alrededor de todo el texto.
                        ("neon", true) => format!("\\c{active}\\3c{white}\\bord{}\\blur3{pop}", cfg.outline_width.max(4)),
                        ("neon", false) => format!("\\c{normal}\\3c{active}\\bord{}\\blur3", cfg.outline_width.max(4)),
                        (_, true) => format!("\\c{active}{pop}"),
                        (_, false) => format!("\\c{normal}"),
                    };
                    format!("{{{tags}}}{text}")
                })
                .collect();

            let mut text = String::new();
            if let Some(size) = layout.font_size {
                text.push_str(&format!("{{\\fs{size}}}"));
            }
            for (j, part) in parts.iter().enumerate() {
                if j > 0 {
                    text.push_str(if layout.break_after == Some(j - 1) { "\\N" } else { " " });
                }
                text.push_str(part);
            }
            lines.push(format!("Dialogue: 0,{},{},Main,,0,0,0,,{text}", format_time(start), format_time(end)));
        }
    }
    Ok(lines)
}

// ----------------------------------------------------------------------------
// Que el bloque quepa: con WrapStyle 2 libass no parte las lineas, asi que un
// bloque de tres palabras largas se salia del video por los dos lados.
// ----------------------------------------------------------------------------

/// Como se dibuja un bloque para que quepa en el ancho.
#[derive(Debug, Default, PartialEq)]
struct GroupLayout {
    /// Se parte en dos lineas despues de la palabra con este indice.
    break_after: Option<usize>,
    /// Cuerpo de letra reducido si ni partido cabe.
    font_size: Option<i64>,
}

/// Ancho medio de una letra, en fracciones del cuerpo, segun la fuente. Medido
/// con libass sobre Arial Black (~0,53) y con un margen: mejor encoger un poco
/// de mas que salirse del video.
fn glyph_ratio(font: &str, uppercase: bool) -> f64 {
    let font = font.to_lowercase();
    let narrow = font.contains("impact") || font.contains("anton") || font.contains("bebas");
    match (narrow, uppercase) {
        (true, true) => 0.46,
        (true, false) => 0.40,
        (false, true) if font.contains("montserrat") => 0.62,
        (false, true) => 0.60,
        (false, false) => 0.52,
    }
}

fn fit_group(words: &[String], cfg: &SubtitlesConfig, width: i64) -> GroupLayout {
    if words.is_empty() || cfg.font_size <= 0 {
        return GroupLayout::default();
    }
    let size = cfg.font_size as f64;
    let ratio = glyph_ratio(&cfg.font_name, cfg.uppercase);
    // Margenes laterales del estilo (7 % a cada lado) y un respiro para el pop
    // de la palabra activa y el contorno.
    let available = width as f64 * 0.86 * 0.93;
    let measure = |range: &[String]| -> f64 {
        let letters: usize = range.iter().map(|w| w.chars().count()).sum();
        let gaps = range.len().saturating_sub(1);
        letters as f64 * ratio * size + gaps as f64 * 0.3 * size
    };

    let whole = measure(words);
    if whole <= available {
        return GroupLayout::default();
    }

    // Dos lineas, cortando donde queden mas parejas.
    let mut best: Option<(usize, f64)> = None;
    for cut in 1..words.len() {
        let widest = measure(&words[..cut]).max(measure(&words[cut..]));
        if best.map_or(true, |(_, w)| widest < w) {
            best = Some((cut - 1, widest));
        }
    }
    let (break_after, widest) = match best {
        Some((index, widest)) => (Some(index), widest),
        None => (None, whole),
    };
    let font_size = (widest > available).then(|| ((size * available / widest) as i64).max((size * 0.4) as i64));
    GroupLayout { break_after, font_size }
}

/// Tarjeta de contexto al arrancar el video. Aparece en el centro, se lee, se
/// va. Su unico trabajo es que el que llega de golpe sepa que esta viendo antes
/// de decidir si sigue mirando.
fn title_event(text: &str, cfg: &TitleCardConfig, duration: f64, subs: Option<&SubtitlesConfig>) -> Vec<String> {
    let end = if duration != 0.0 { cfg.duration.min(duration) } else { cfg.duration };
    if end <= 0.0 {
        return Vec::new();
    }

    let mut clean = take_chars(&censor_text(text.trim(), subs), cfg.max_chars);
    if cfg.uppercase {
        clean = clean.to_uppercase();
    }
    let body = escape_ass_text(&wrap(&clean, cfg.wrap_chars));

    let fade = (cfg.fade.max(0.0) * 1000.0) as i64;
    // Entra con el mismo rebote que la palabra activa de los subtitulos, para
    // que se lea como parte del mismo diseno y no como un rotulo aparte.
    let effect = format!("{{\\fscx88\\fscy88\\t(0,200,\\fscx103\\fscy103)\\t(200,320,\\fscx100\\fscy100)\\fad({fade},{fade})}}");
    vec![format!("Dialogue: 2,{},{},Titulo,,0,0,0,,{effect}{body}", format_time(0.0), format_time(end))]
}

fn hook_event(text: &str, cfg: &HookConfig, duration: f64, start: f64, subs: Option<&SubtitlesConfig>) -> Vec<String> {
    let end = if duration != 0.0 { (start + cfg.duration).min(duration) } else { start + cfg.duration };
    if end <= start {
        return Vec::new();
    }

    let mut clean = take_chars(&censor_text(text.trim(), subs), cfg.max_chars);
    if cfg.uppercase {
        clean = clean.to_uppercase();
    }
    let body = escape_ass_text(&wrap(&clean, 24));

    // Entrada con un pequeno rebote y salida por desvanecido.
    let effect = "{\\fscx70\\fscy70\\t(0,180,\\fscx104\\fscy104)\\t(180,300,\\fscx100\\fscy100)\\fad(0,350)}";
    vec![format!("Dialogue: 1,{},{},Hook,,0,0,0,,{effect}{body}", format_time(start), format_time(end))]
}

fn render_word(word: &Word, cfg: &SubtitlesConfig) -> String {
    let mut text = word.text.trim().to_string();
    if cfg.remove_punctuation {
        text = text.chars().filter(|c| !cfg.punctuation_chars.contains(*c)).collect::<String>().trim().to_string();
    }
    if text.is_empty() {
        text = word.text.trim().to_string();
    }
    if cfg.uppercase {
        text = text.to_uppercase();
    }
    escape_ass_text(&censor(&text, &cfg.censor_words, cfg.censor_profanity, &cfg.censor_char))
}

fn take_chars(text: &str, count: i64) -> String {
    text.chars().take(count.max(0) as usize).collect()
}

// ----------------------------------------------------------------------------
// Censura de palabrotas
// ----------------------------------------------------------------------------
//
// Se tapa UNA letra, la primera vocal de la raiz malsonante: "puta" -> "p#ta",
// "gilipollas" -> "gilip#llas". La palabra se sigue leyendo de un vistazo (que
// es lo que se quiere: el clip no pierde gracia) pero deja de ser texto literal
// para los filtros automaticos de las plataformas. Solo afecta al subtitulo: el
// audio no se toca.

/// Raices, no palabras: asi "puta", "putas", "putada" y "putazo" caen las
/// cuatro con una sola entrada.
const PROFANITY_ROOTS: &[&str] = &[
    "puta", "puto", "pute", "polla", "pollez", "mierd", "jod", "coñ", "cabron", "cabron", "hostia", "folla",
    "follar", "follo", "capull", "subnormal", "maric", "zorra", "cojon", "cag", "gilipoll", "pendejo", "chinga",
    "verga", "boludo", "mamon", "gonorrea", "carajo", "cornud", "fuck", "shit", "bitch", "asshole", "cunt", "dick",
];

/// La raiz vale si la palabra empieza por ella o por uno de estos refuerzos. La
/// lista es corta a proposito: con "re" dentro, "reputacion" se convertiria en
/// "rep#tacion", que es justo lo que hay que evitar.
const PROFANITY_PREFIXES: &[&str] = &["hijo", "hija", "hijode", "gili", "mala", "malla"];

/// Palabras inocentes que empiezan por una de las raices.
const NOT_PROFANITY: &[&str] = &["follaje", "follajes", "coñac", "coñacs"];

const VOWELS: &str = "aeiouáéíóúü";

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Minusculas y sin tildes, pero CONSERVANDO la enye: si se quitase, "coño"
/// pasaria a "cono" y la raiz "coñ" empezaria a marcar "conocer".
fn fold_word(word: &str) -> String {
    word.to_lowercase()
        .chars()
        .map(|c| match c {
            'á' => 'a',
            'é' => 'e',
            'í' => 'i',
            'ó' => 'o',
            'ú' | 'ü' => 'u',
            other => other,
        })
        .collect()
}

/// Posicion y raiz malsonante dentro de la palabra, si la hay.
fn find_root(word: &str) -> Option<(usize, String)> {
    let folded = fold_word(word);
    if NOT_PROFANITY.contains(&folded.as_str()) {
        return None;
    }
    for root in PROFANITY_ROOTS {
        if folded.starts_with(root) {
            return Some((0, root.to_string()));
        }
        for prefix in PROFANITY_PREFIXES {
            if folded.starts_with(&format!("{prefix}{root}")) {
                return Some((prefix.chars().count(), root.to_string()));
            }
        }
    }
    None
}

/// Tapa la primera vocal de la raiz malsonante de la palabra.
fn censor(text: &str, extra: &[String], enabled: bool, mask: &str) -> String {
    let bare: String = text.chars().filter(|c| is_word_char(*c)).collect();
    if bare.is_empty() {
        return text.to_string();
    }

    let mut found = if enabled { find_root(&bare) } else { None };
    if found.is_none() {
        // Las palabras que anada el usuario se tratan como raices tambien, para
        // que "cabronazo" caiga si ha escrito "cabron".
        let folded = fold_word(&bare);
        for word in extra {
            let root = fold_word(&word.chars().filter(|c| is_word_char(*c)).collect::<String>());
            if !root.is_empty() && folded.starts_with(&root) {
                found = Some((0, root));
                break;
            }
        }
    }
    let Some((offset, root)) = found else { return text.to_string() };

    // Primera vocal DENTRO de la raiz: es la silaba que hace reconocible la
    // palabrota, y taparla ahi es lo que se lee como censura deliberada.
    let vowel = root.chars().position(|c| VOWELS.contains(c)).unwrap_or(0);
    let target = offset + vowel;

    // `text` puede llevar signos de puntuacion delante ("¡puta"): hay que
    // traducir el indice de la palabra desnuda al indice del texto real.
    let mut seen: i64 = -1;
    for (byte_index, ch) in text.char_indices() {
        if is_word_char(ch) {
            seen += 1;
            if seen as usize == target {
                return format!("{}{mask}{}", &text[..byte_index], &text[byte_index + ch.len_utf8()..]);
            }
        }
    }
    text.to_string()
}

/// Aplica la censura a una frase entera, palabra por palabra. El rotulo y la
/// tarjeta de titulo salen de la transcripcion igual que los subtitulos, asi
/// que tienen exactamente el mismo problema.
pub fn censor_text(text: &str, subs: Option<&SubtitlesConfig>) -> String {
    let Some(subs) = subs else { return text.to_string() };
    text.split(' ').map(|chunk| censor(chunk, &subs.censor_words, subs.censor_profanity, &subs.censor_char)).collect::<Vec<_>>().join(" ")
}

/// Parte el texto en lineas de como mucho `width` caracteres. Los saltos de
/// linea que ya trae el texto se respetan: hay sitios donde el corte no es
/// cuestion de anchura sino de sentido (el programa arriba y el invitado debajo).
pub fn wrap(text: &str, width: i64) -> String {
    if text.contains('\n') {
        return text.split('\n').filter(|p| !p.trim().is_empty()).map(|part| wrap(part, width)).collect::<Vec<_>>().join("\n");
    }
    let width = width.max(0) as usize;
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        let candidate = if current.is_empty() { word.to_string() } else { format!("{current} {word}") };
        if candidate.chars().count() > width && !current.is_empty() {
            lines.push(std::mem::take(&mut current));
            current = word.to_string();
        } else {
            current = candidate;
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines.join("\n")
}

// ----------------------------------------------------------------------------
// Utilidades de formato .ASS
// ----------------------------------------------------------------------------

/// #RRGGBB -> &HAABBGGRR& (el .ASS invierte el orden de los bytes). `alpha`
/// va de 0 (opaco) a 255 (invisible), al reves que en CSS.
pub fn hex_to_ass_color(hex: &str, alpha: i64) -> Result<String, SubtitleError> {
    let value = hex.trim_start_matches('#');
    if value.chars().count() != 6 || !value.is_ascii() {
        return Err(SubtitleError(format!("Color invalido: '{hex}'. Usa el formato #RRGGBB.")));
    }
    let (r, g, b) = (&value[0..2], &value[2..4], &value[4..6]);
    Ok(format!("&H{:02X}{b}{g}{r}&", alpha.clamp(0, 255)).to_uppercase())
}

/// Segundos -> H:MM:SS.CS, el formato de tiempo del .ASS.
pub fn format_time(seconds: f64) -> String {
    let total = ((seconds * 100.0).round_ties_even() as i64).max(0);
    let (hours, rest) = (total / 360_000, total % 360_000);
    let (minutes, rest) = (rest / 6_000, rest % 6_000);
    let (secs, centis) = (rest / 100, rest % 100);
    format!("{hours}:{minutes:02}:{secs:02}.{centis:02}")
}

/// Neutraliza los caracteres que romperian la sintaxis del .ASS.
pub fn escape_ass_text(text: &str) -> String {
    text.replace('\\', "").replace('{', "(").replace('}', ")").replace('\r', "").replace('\n', "\\N")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn palabras(spans: &[(f64, f64)]) -> Vec<Word> {
        spans.iter().enumerate().map(|(i, &(a, b))| Word::new(format!("p{i}"), a, b)).collect()
    }

    fn doce() -> Vec<Word> {
        palabras(&(0..12).map(|i| (i as f64 * 0.5, i as f64 * 0.5 + 0.4)).collect::<Vec<_>>())
    }

    fn construir() -> String {
        build_ass(
            &doce(), 1080, 1920, &SubtitlesConfig::default(), Some(&HookConfig::default()), Some("UN GANCHO"), 6.0,
            Some(&TitleCardConfig::default()), Some("El Programa\nInvitado"),
        )
        .unwrap()
    }

    // -- formato -------------------------------------------------------------

    #[test]
    fn tiempo_en_el_formato_del_ass() {
        assert_eq!(format_time(0.0), "0:00:00.00");
        assert_eq!(format_time(61.5), "0:01:01.50");
        assert_eq!(format_time(3661.23), "1:01:01.23");
        assert_eq!(format_time(-3.0), "0:00:00.00", "los tiempos negativos no rompen el fichero");
    }

    #[test]
    fn color_invertido_como_pide_el_formato() {
        assert_eq!(hex_to_ass_color("#FFF200", 0).unwrap(), "&H0000F2FF&");
        assert_eq!(hex_to_ass_color("#000000", 0).unwrap(), "&H00000000&");
        assert_eq!(hex_to_ass_color("ff0000", 128).unwrap(), "&H800000FF&");
        assert!(hex_to_ass_color("rojo", 0).is_err());
    }

    #[test]
    fn el_texto_no_rompe_la_sintaxis() {
        assert_eq!(escape_ass_text("a{b}c\\d\r\ne"), "a(b)cd\\Ne");
    }

    // -- agrupacion ----------------------------------------------------------

    #[test]
    fn los_bloques_respetan_el_maximo_de_palabras() {
        let cfg = SubtitlesConfig { max_words_per_group: 3, ..Default::default() };
        let habla = palabras(&(0..10).map(|i| (i as f64 * 0.4, i as f64 * 0.4 + 0.3)).collect::<Vec<_>>());
        for bloque in group_words(&habla, &cfg) {
            assert!(bloque.words.len() <= 3);
        }
    }

    #[test]
    fn una_pausa_larga_o_un_punto_abre_bloque_nuevo() {
        let cfg = SubtitlesConfig { max_words_per_group: 9, ..Default::default() };
        assert!(group_words(&palabras(&[(0.0, 0.3), (0.4, 0.7), (5.0, 5.3)]), &cfg).len() >= 2);

        let mut con_punto = palabras(&[(0.0, 0.3), (0.4, 0.7), (0.8, 1.1)]);
        con_punto[0].text = "hola.".into();
        assert_eq!(group_words(&con_punto, &cfg).len(), 2);
        assert!(group_words(&[], &cfg).is_empty());
    }

    // -- tiempos de whisper ---------------------------------------------------

    fn dialogos(contenido: &str) -> Vec<(f64, f64, String)> {
        let segundos = |t: &str| -> f64 {
            let p: Vec<f64> = t.split(':').map(|x| x.parse().unwrap()).collect();
            p[0] * 3600.0 + p[1] * 60.0 + p[2]
        };
        contenido
            .lines()
            .filter(|l| l.starts_with("Dialogue: 0,"))
            .map(|l| {
                let p: Vec<&str> = l.splitn(10, ',').collect();
                (segundos(p[1]), segundos(p[2]), p[9].to_string())
            })
            .collect()
    }

    fn texto(lista: &[(&str, f64, f64)]) -> Vec<Word> {
        lista.iter().map(|&(t, a, b)| Word::new(t, a, b)).collect()
    }

    #[test]
    fn dos_bloques_nunca_estan_a_la_vez_en_pantalla() {
        // Caso real: el bloque siguiente arrancaba 20 ms antes de que acabase
        // el anterior y libass subia uno de los dos: el subtitulo daba un salto.
        let cfg = SubtitlesConfig { max_words_per_group: 2, ..Default::default() };
        let habla = texto(&[("feeling", 2.28, 2.90), ("watched", 2.90, 3.38), ("man", 3.38, 3.68), ("spawning", 3.68, 4.47)]);
        let ass = build_ass(&habla, 1080, 1920, &cfg, None, None, 6.0, None, None).unwrap();
        let lineas = dialogos(&ass);
        for par in lineas.windows(2) {
            assert!(par[0].1 <= par[1].0 + 1e-9, "se solapan: {:?} y {:?}", par[0], par[1]);
        }
    }

    #[test]
    fn las_palabras_sin_duracion_se_reparten_el_tiempo_y_todas_se_resaltan() {
        // Caso real: "VALE ESE SONIDO POR" con los cuatro en el mismo instante.
        let cfg = SubtitlesConfig { max_words_per_group: 4, ..Default::default() };
        let habla = texto(&[("vale", 19.97, 19.97), ("ese", 19.97, 19.97), ("sonido", 19.97, 19.97), ("por", 19.97, 20.06), ("lo", 20.37, 20.6)]);
        let ass = build_ass(&habla, 1080, 1920, &cfg, None, None, 30.0, None, None).unwrap();
        let lineas = dialogos(&ass);
        assert!((lineas[0].0 - 19.97).abs() < 0.011);
        let activas = |lineas: &[(f64, f64, String)]| -> Vec<String> {
            // La palabra que va detras del color activo, sin etiquetas ni saltos.
            lineas
                .iter()
                .filter_map(|(_, _, t)| t.split("&H0000F2FF&").nth(1)?.split('}').nth(1)?.split('{').next())
                .map(|w| w.replace("\\N", "").trim().to_string())
                .collect()
        };
        assert_eq!(activas(&lineas)[..4], ["VALE", "ESE", "SONIDO", "POR"], "{lineas:?}");

        // Y sin sitio despues (la siguiente palabra 70 ms mas tarde): usan el
        // hueco de antes, sin pisar el "no" anterior. Antes no salian.
        let cfg = SubtitlesConfig { max_words_per_group: 3, ..Default::default() };
        let apretado = texto(&[("no", 18.88, 19.78), ("vale", 19.97, 19.97), ("ese", 19.97, 19.97), ("sonido", 19.97, 19.97), ("por", 19.97, 20.04), ("lo", 20.04, 20.38)]);
        let lineas = dialogos(&build_ass(&apretado, 1080, 1920, &cfg, None, None, 30.0, None, None).unwrap());
        assert_eq!(activas(&lineas)[..6], ["NO", "VALE", "ESE", "SONIDO", "POR", "LO"], "{lineas:?}");
        assert!(lineas[1].0 >= 19.78 - 1e-9, "no pisa el 'no': {lineas:?}");
        assert!(lineas[1..5].iter().all(|(a, b, _)| b - a >= 0.04), "{lineas:?}");
    }

    #[test]
    fn el_reparto_de_tiempos_no_toca_lo_que_ya_esta_bien() {
        let bien = texto(&[("uno", 0.0, 0.3), ("dos", 0.35, 0.6), ("tres", 0.7, 1.0)]);
        assert_eq!(tidy_words(&bien), bien);
    }

    #[test]
    fn la_palabra_repetida_en_el_borde_de_dos_segmentos_sale_una_vez() {
        let limpio = tidy_words(&texto(&[("hierro", 2.41, 2.95), ("a", 2.95, 3.05), ("A", 2.95, 3.03), ("mecista", 3.03, 3.35)]));
        let textos: Vec<&str> = limpio.iter().map(|w| w.text.as_str()).collect();
        assert_eq!(textos, ["hierro", "a", "mecista"]);
        // Y nunca va hacia atras.
        let raro = tidy_words(&texto(&[("uno", 1.0, 1.5), ("dos", 0.8, 1.2), ("tres", 1.6, 2.0)]));
        assert!(raro.windows(2).all(|p| p[0].start <= p[1].start));
        assert!(raro.iter().all(|w| w.end >= w.start));
    }

    // -- fichero completo ----------------------------------------------------

    #[test]
    fn el_gancho_empieza_cuando_se_va_la_tarjeta() {
        let contenido = construir();
        let tarjeta: Vec<&str> = contenido.lines().filter(|l| l.contains(",Titulo,")).collect();
        let gancho: Vec<&str> = contenido.lines().filter(|l| l.contains(",Hook,")).collect();
        assert_eq!((tarjeta.len(), gancho.len()), (1, 1));

        let fin_tarjeta = tarjeta[0].split(',').nth(2).unwrap();
        let inicio_gancho = gancho[0].split(',').nth(1).unwrap();
        assert_eq!(fin_tarjeta, inicio_gancho);
        assert_eq!(inicio_gancho, format_time(TitleCardConfig::default().duration));
    }

    #[test]
    fn los_subtitulos_no_dejan_huecos_dentro_de_un_bloque() {
        let contenido = construir();
        let dialogos: Vec<&str> = contenido.lines().filter(|l| l.starts_with("Dialogue: 0,")).collect();
        assert!(dialogos.len() > 3);
        for linea in &dialogos {
            let partes: Vec<&str> = linea.split(',').collect();
            assert!(partes[1] < partes[2], "{linea}"); // inicio < fin, siempre
        }
        // Dentro de un bloque cada linea acaba justo donde empieza la siguiente.
        let tiempos: Vec<(String, String)> = dialogos.iter().map(|l| (l.split(',').nth(1).unwrap().into(), l.split(',').nth(2).unwrap().into())).collect();
        assert_eq!(tiempos[0].1, tiempos[1].0);
    }

    #[test]
    fn la_palabra_activa_lleva_el_color_y_el_pop() {
        let contenido = construir();
        let primera = contenido.lines().find(|l| l.starts_with("Dialogue: 0,")).unwrap();
        assert!(primera.contains("{\\c&H0000F2FF&\\fscx118\\fscy118\\t(0,120,\\fscx100\\fscy100)}P0"), "{primera}");
        assert!(primera.contains("{\\c&H00FFFFFF&}P1"));
        let sin_pop = build_ass(&doce(), 1080, 1920, &SubtitlesConfig { pop_scale: 100, ..Default::default() }, None, None, 6.0, None, None).unwrap();
        assert!(!sin_pop.contains("\\fscx"));
    }

    #[test]
    fn la_cabecera_trae_los_estilos_que_se_usan() {
        let c = construir();
        assert!(c.contains("PlayResX: 1080") && c.contains("PlayResY: 1920"));
        assert!(c.contains("Style: Main,Arial Black,92,") && c.contains(",2,75,75,576,1"));
        assert!(c.contains("Style: Hook,") && c.contains("Style: Titulo,Arial Black,74,"));
        let solo_subs = build_ass(&doce(), 1080, 1920, &SubtitlesConfig::default(), None, None, 6.0, None, None).unwrap();
        assert!(!solo_subs.contains("Style: Hook") && !solo_subs.contains("Style: Titulo"));
    }

    #[test]
    fn sin_gancho_ni_tarjeta_solo_hay_subtitulos() {
        let c = build_ass(&doce(), 1080, 1920, &SubtitlesConfig::default(), Some(&HookConfig::default()), None, 6.0, Some(&TitleCardConfig::default()), Some("")).unwrap();
        assert!(!c.contains(",Hook,") && !c.contains(",Titulo,"));
    }

    #[test]
    fn la_tarjeta_y_el_gancho_no_pasan_de_la_duracion_del_clip() {
        let c = build_ass(&[], 1080, 1920, &SubtitlesConfig::default(), Some(&HookConfig::default()), Some("hola"), 3.0, Some(&TitleCardConfig::default()), Some("x")).unwrap();
        let gancho = c.lines().find(|l| l.contains(",Hook,")).unwrap();
        assert_eq!(gancho.split(',').nth(2).unwrap(), "0:00:03.00");
        // Si la tarjeta dura mas que el clip, el gancho no tiene sitio y no sale.
        let corto = build_ass(&[], 1080, 1920, &SubtitlesConfig::default(), Some(&HookConfig::default()), Some("hola"), 2.0, Some(&TitleCardConfig::default()), Some("x")).unwrap();
        assert!(!corto.contains(",Hook,"));
    }

    #[test]
    fn un_color_malo_es_un_error_y_no_un_fichero_roto() {
        let cfg = SubtitlesConfig { color_active: "amarillo".into(), ..Default::default() };
        assert!(build_ass(&doce(), 1080, 1920, &cfg, None, None, 6.0, None, None).is_err());
    }

    #[test]
    fn el_rotulo_va_en_mayusculas_censurado_y_en_lineas_cortas() {
        let c = build_ass(&[], 1080, 1920, &SubtitlesConfig::default(), Some(&HookConfig::default()), Some("no me jodas que esto es una locura enorme"), 8.0, None, None).unwrap();
        let gancho = c.lines().find(|l| l.contains(",Hook,")).unwrap();
        assert!(gancho.contains("J#DAS"), "{gancho}");
        assert!(gancho.contains("\\N"), "debe partirse en lineas: {gancho}");
        assert!(gancho.contains("NO ME"));
    }

    // -- censura y texto -----------------------------------------------------

    fn tapar(texto: &str) -> String {
        censor(texto, &[], true, "#")
    }

    #[test]
    fn se_tapa_la_primera_vocal_de_la_raiz() {
        assert_eq!(tapar("puta"), "p#ta");
        assert_eq!(tapar("PUTADA"), "P#TADA");
        assert_eq!(tapar("mierda"), "m#erda");
        assert_eq!(tapar("hijoputa"), "hijop#ta");
        assert_eq!(tapar("gilipollas"), "gilip#llas");
        assert_eq!(tapar("¡puta!"), "¡p#ta!");
        assert_eq!(tapar("fuck"), "f#ck");
    }

    #[test]
    fn lo_inocente_no_se_toca() {
        for texto in ["reputacion", "computadora", "conocer", "con", "follaje", "coñac", "hola", "", "...", "dispute"] {
            assert_eq!(tapar(texto), texto, "{texto}");
        }
        assert_eq!(tapar("coño"), "c#ño");
    }

    #[test]
    fn la_censura_se_puede_apagar_y_ampliar() {
        assert_eq!(censor("puta", &[], false, "#"), "puta");
        assert_eq!(censor("cabronazo", &["tonto".into()], false, "#"), "cabronazo");
        assert_eq!(censor("tontorron", &["Tonto".into()], false, "#"), "t#ntorron");
        assert_eq!(censor("tontisimo", &["Tonto".into()], false, "#"), "tontisimo", "no empieza por la raiz");
        assert_eq!(censor("puta", &[], true, "*"), "p*ta");
    }

    #[test]
    fn censurar_una_frase_entera() {
        let cfg = SubtitlesConfig::default();
        assert_eq!(censor_text("no me jodas, que puta locura", Some(&cfg)), "no me j#das, que p#ta locura");
        assert_eq!(censor_text("no me jodas", None), "no me jodas");
    }

    #[test]
    fn la_puntuacion_se_quita_pero_una_palabra_que_solo_es_puntuacion_se_queda() {
        let cfg = SubtitlesConfig::default();
        assert_eq!(render_word(&Word::new("¿Hola,", 0.0, 1.0), &cfg), "HOLA");
        assert_eq!(render_word(&Word::new("...", 0.0, 1.0), &cfg), "...");
        let sin_quitar = SubtitlesConfig { remove_punctuation: false, uppercase: false, ..Default::default() };
        assert_eq!(render_word(&Word::new("¿Hola,", 0.0, 1.0), &sin_quitar), "¿Hola,");
    }

    fn con_estilo(estilo: &str) -> String {
        let cfg = SubtitlesConfig { style: estilo.into(), ..SubtitlesConfig::default() };
        build_ass(&doce(), 1080, 1920, &cfg, None, None, 6.0, None, None).unwrap()
    }

    #[test]
    fn cada_plantilla_cambia_el_estilo_y_las_etiquetas() {
        let clasico = con_estilo("classic");
        assert!(clasico.contains(",-1,0,0,0,100,100,0,0,1,5,2,2,"), "borde 1, contorno 5, sombra 2: {clasico}");

        let caja = con_estilo("box");
        assert!(caja.contains(",0,0,3,14,0,2,"), "BorderStyle 3 con relleno 14: {caja}");
        assert!(caja.contains("&H6E000000&"), "caja translucida");

        let rotulador = con_estilo("marker");
        assert!(rotulador.contains("\\3c&H0000F2FF&\\bord14"), "la palabra que suena lleva el bloque amarillo: {rotulador}");
        assert!(rotulador.contains("\\c&H00000000&"), "con el texto en negro");

        let neon = con_estilo("neon");
        assert!(neon.contains("\\blur3") && neon.contains("\\3c&H0000F2FF&"), "{neon}");

        let minimo = con_estilo("minimal");
        assert!(minimo.contains(",0,0,1,2,1,2,"), "contorno 2, sombra 1: {minimo}");
        assert!(!minimo.contains("\\fscx118"), "sin pop");

        // Una plantilla desconocida se queda como la clasica.
        assert_eq!(con_estilo("inventada"), clasico);
    }

    #[test]
    fn el_usuario_del_canal_se_pega_junto_al_logo() {
        let subs = SubtitlesConfig::default();
        let mut marca = BrandConfig { position: "tr".into(), handle: "  @micanal ".into(), ..BrandConfig::default() };
        let mut ass = String::new();
        append_handle(&mut ass, &marca, 1080, 1920, 10.0, &subs, Some((151, 100)));
        assert!(ass.contains("\\an9\\pos(1038,152)") && ass.contains("}@micanal\n"), "debajo del logo, a la derecha: {ass}");

        let mut ass = String::new();
        marca.position = "bl".into();
        append_handle(&mut ass, &marca, 1080, 1920, 10.0, &subs, None);
        assert!(ass.contains("\\an1\\pos(42,1878)"), "{ass}");

        let mut vacio = String::new();
        marca.handle = "  ".into();
        append_handle(&mut vacio, &marca, 1080, 1920, 10.0, &subs, None);
        assert!(vacio.is_empty());
    }

    #[test]
    fn solo_se_pitan_las_palabras_que_la_censura_tapa() {
        let subs = SubtitlesConfig::default();
        let words = vec![Word::new("hola", 0.0, 0.4), Word::new("puta", 1.0, 1.4), Word::new("madre", 1.5, 1.9), Word::new("joder", 3.0, 3.3)];
        let tramos = censored_intervals(&words, &subs);
        assert_eq!(tramos.len(), 2, "{tramos:?}");
        assert!((tramos[0].0 - 0.96).abs() < 1e-9 && (tramos[0].1 - 1.46).abs() < 1e-9, "con margen: {tramos:?}");
        // Si la censura esta apagada no se pita nada.
        let sin = SubtitlesConfig { censor_profanity: false, ..SubtitlesConfig::default() };
        assert!(censored_intervals(&words, &sin).is_empty());
    }

    #[test]
    fn la_tarjeta_final_oscurece_y_pone_el_texto_al_final() {
        let subs = SubtitlesConfig::default();
        let tarjeta = crate::config::EndCardConfig { enabled: true, text: "Sígueme para más".into(), duration: 2.5 };
        let mut ass = String::new();
        append_end_card(&mut ass, &tarjeta, "@micanal", 1080, 1920, 20.0, &subs);
        assert!(ass.contains("Dialogue: 4,0:00:17.50,0:00:20.00"), "los ultimos 2,5 s: {ass}");
        assert!(ass.contains(r"\p1}m 0 0 l 1080 0 1080 1920 0 1920"), "rectangulo oscuro: {ass}");
        assert!(ass.contains(r"SÍGUEME PARA\NMÁS") || ass.contains("SÍGUEME PARA MÁS"), "{ass}");
        assert!(ass.contains("@micanal"), "el usuario va debajo: {ass}");
        // Apagada, o en un clip demasiado corto, no escribe nada.
        let mut vacio = String::new();
        append_end_card(&mut vacio, &crate::config::EndCardConfig { enabled: false, ..tarjeta.clone() }, "", 1080, 1920, 20.0, &subs);
        append_end_card(&mut vacio, &tarjeta, "", 1080, 1920, 0.8, &subs);
        assert!(vacio.is_empty());
    }

    #[test]
    fn la_portada_lleva_el_gancho_en_mayusculas_y_arriba() {
        let ass = cover_ass("¿Por qué nadie lo dice?", 1080, 1350, &SubtitlesConfig::default(), &HookConfig::default());
        assert!(ass.contains("PlayResY: 1350") && ass.contains("Style: Cover,Arial Black,"), "{ass}");
        assert!(ass.contains("Dialogue: 0,0:00:00.00,0:00:10.00,Cover,,0,0,0,,¿POR QUÉ"), "{ass}");
        assert!(ass.contains(",8,75,75,189,1"), "alineado arriba con margen: {ass}");
    }

    fn bloque(textos: &[&str]) -> Vec<String> {
        textos.iter().map(|t| t.to_string()).collect()
    }

    #[test]
    fn un_bloque_corto_no_se_toca() {
        let cfg = SubtitlesConfig::default();
        assert_eq!(fit_group(&bloque(&["ESTO", "NO", "TE"]), &cfg, 1080), GroupLayout::default());
    }

    #[test]
    fn tres_palabras_largas_se_parten_en_dos_lineas() {
        let cfg = SubtitlesConfig::default();
        let layout = fit_group(&bloque(&["EVIDENTEMENTE", "CUANDO", "CONOCES"]), &cfg, 1080);
        assert_eq!(layout.break_after, Some(0));
    }

    #[test]
    fn una_palabra_enorme_encoge_el_cuerpo() {
        let cfg = SubtitlesConfig::default();
        let layout = fit_group(&bloque(&["ANTICONSTITUCIONALMENTE"]), &cfg, 1080);
        assert_eq!(layout.break_after, None);
        let size = layout.font_size.expect("tiene que encoger");
        assert!(size < cfg.font_size && size as f64 >= cfg.font_size as f64 * 0.4);
    }

    #[test]
    fn el_ass_lleva_el_salto_y_el_tamano_en_todas_las_lineas_del_bloque() {
        let words = vec![Word::new("evidentemente", 0.0, 0.4), Word::new("cuando", 0.5, 0.9), Word::new("conoces", 1.0, 1.4)];
        let ass = build_ass(&words, 1080, 1920, &SubtitlesConfig::default(), None, None, 2.0, None, None).unwrap();
        let dialogos: Vec<&str> = ass.lines().filter(|l| l.starts_with("Dialogue: 0")).collect();
        assert_eq!(dialogos.len(), 3);
        assert!(dialogos.iter().all(|l| l.contains("\\N")), "{ass}");
    }

    /// A mano: escribe un .ASS con palabras largas en la ruta de VS_ASS_OUT para
    /// mirarlo renderizado (`ffmpeg -vf ass=...`). `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn escribe_un_ass_de_ejemplo() {
        let Ok(salida) = std::env::var("VS_ASS_OUT") else { return };
        let textos = ["evidentemente", "cuando", "conoces", "esto", "no", "te", "lo", "habian", "contado", "anticonstitucionalmente"];
        let words: Vec<Word> = textos.iter().enumerate().map(|(i, t)| Word::new(*t, i as f64, i as f64 + 0.9)).collect();
        let ass = build_ass(&words, 1080, 1920, &SubtitlesConfig::default(), None, None, 11.0, None, None).unwrap();
        std::fs::write(salida, ass).unwrap();
    }

    #[test]
    fn el_ajuste_de_lineas() {
        assert_eq!(wrap("uno dos tres cuatro", 8), "uno dos\ntres\ncuatro");
        assert_eq!(wrap("palabralarguisima corta", 5), "palabralarguisima\ncorta");
        assert_eq!(wrap("El Programa\nInvitado", 22), "El Programa\nInvitado");
        assert_eq!(wrap("", 10), "");
    }
}
