//! Plan de montaje: la seleccion de clips, guardada para revisarla.
//!
//! Elegir los momentos cuesta minutos (descarga, transcripcion, Claude);
//! renderizarlos cuesta otros tantos. Antes las dos cosas iban pegadas, asi que
//! para cambiar un titulo o descartar un clip malo habia que repetirlo todo.
//!
//! El plan parte el pipeline en dos. La primera mitad escribe aqui QUE se va a
//! montar -- con las palabras y sus tiempos, que es lo unico que el render
//! necesita de la transcripcion -- y la segunda mitad lo lee. En medio cabe una
//! persona: marcar los clips que valen, reescribir un gancho, cambiar el color de
//! los subtitulos, y montar solo eso.
//!
//! El fichero lleva ademas la configuracion completa con la que se eligio, de
//! modo que el render reproduce los mismos ajustes aunque config.yaml haya
//! cambiado por el camino. Tiene el mismo formato que el de la version Python.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::config::Config;
use crate::download::SourceVideo;
use crate::llm::ClipMetadata;
use crate::scoring::{hms, Window};
use crate::words::Word;

pub const FORMAT_VERSION: i64 = 2;

#[derive(Debug, thiserror::Error)]
pub enum PlanError {
    #[error("No existe el plan: {0}")]
    NotFound(String),
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Io(String),
}

/// Lo poco que el render necesita de un video ya descargado.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PlanSource {
    pub ruta: String,
    pub titulo: String,
    pub duracion: f64,
    pub url: String,
}

impl PlanSource {
    pub fn from_video(video: &SourceVideo) -> Self {
        Self {
            ruta: std::path::absolute(&video.path).unwrap_or_else(|_| video.path.clone()).to_string_lossy().into_owned(),
            titulo: video.title.clone(),
            duracion: round_to(video.duration, 2),
            url: video.webpage_url().to_string(),
        }
    }
}

/// El clip tal y como salio de la seleccion, antes de recortarlo o corregirle el
/// texto: permite volver atras y recortar desde el principio.
#[derive(Debug, Clone, PartialEq)]
pub struct OriginalClip {
    pub start: f64,
    pub end: f64,
    pub text: String,
    pub words: Vec<Word>,
}

/// Un candidato del plan, con todo lo que hace falta para montarlo.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanClip {
    pub start: f64,
    pub end: f64,
    pub score: f64,
    pub reason: String,
    pub text: String,
    pub selected: bool,
    pub signals: HashMap<String, f64>,
    pub meta: Option<ClipMetadata>,
    pub words: Vec<Word>,
    /// Plantilla de subtitulos propia de este clip.
    pub style: String,
    /// Lo que habia antes de editar el clip, si se ha editado.
    pub original: Option<OriginalClip>,
}

impl PlanClip {
    pub fn duration(&self) -> f64 {
        self.end - self.start
    }

    /// Reconstruye la ventana que espera el render.
    pub fn to_window(&self) -> Window {
        let signal = |name: &str| self.signals.get(name).copied().unwrap_or(0.0);
        Window {
            start: self.start,
            end: self.end,
            words: self.words.clone(),
            text: self.text.clone(),
            heat: signal("heat"),
            energy: signal("energy"),
            density: signal("density"),
            context: signal("context"),
            llm_content: signal("llm_content"),
            llm_hook: signal("llm_hook"),
            reason: self.reason.clone(),
            score: self.score,
            style: self.style.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Plan {
    pub path: PathBuf,
    pub config: Value,
    pub podcast: PlanSource,
    pub background: PlanSource,
    pub clips: Vec<PlanClip>,
}

impl Plan {
    /// Indices (desde 0) de los clips marcados.
    pub fn selected(&self) -> Vec<usize> {
        self.clips.iter().enumerate().filter(|(_, c)| c.selected).map(|(i, _)| i).collect()
    }
}

fn round_to(value: f64, places: i32) -> f64 {
    let factor = 10f64.powi(places);
    (value * factor).round_ties_even() / factor
}

// ----------------------------------------------------------------------------
// Escritura
// ----------------------------------------------------------------------------

/// Guarda la seleccion. `selected` = cuantos vienen marcados de serie.
pub fn save(
    path: &Path,
    cfg: &Config,
    podcast: &PlanSource,
    background: &PlanSource,
    clips: &[Window],
    metadata: &HashMap<usize, ClipMetadata>,
    selected: usize,
) -> Result<PathBuf, PlanError> {
    let data = json!({
        "version": FORMAT_VERSION,
        "creado": chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        "config": cfg.to_value(),
        "podcast": podcast,
        "background": background,
        "clips": clips.iter().enumerate().map(|(i, clip)| clip_dict(clip, metadata.get(&i), i < selected)).collect::<Vec<_>>(),
    });
    write_json(path, &data)?;
    log::info!("Plan guardado en {} ({} candidatos, {} marcados).", path.display(), clips.len(), selected.min(clips.len()));
    Ok(path.to_path_buf())
}

fn write_json(path: &Path, data: &Value) -> Result<(), PlanError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| PlanError::Io(format!("No se pudo crear {}: {e}", parent.display())))?;
    }
    let text = serde_json::to_string_pretty(data).map_err(|e| PlanError::Io(e.to_string()))?;
    std::fs::write(path, text).map_err(|e| PlanError::Io(format!("No se pudo escribir {}: {e}", path.display())))
}

fn clip_dict(clip: &Window, meta: Option<&ClipMetadata>, selected: bool) -> Value {
    json!({
        "inicio": round_to(clip.start, 3),
        "fin": round_to(clip.end, 3),
        "marca": hms(clip.start),
        "duracion": round_to(clip.duration(), 1),
        "puntuacion": round_to(clip.score, 4),
        "motivo": clip.reason,
        "texto": clip.text,
        "seleccionado": selected,
        "senales": {
            "heat": round_to(clip.heat, 4),
            "energy": round_to(clip.energy, 4),
            "density": round_to(clip.density, 4),
            "context": round_to(clip.context, 4),
            "llm_content": round_to(clip.llm_content, 4),
            "llm_hook": round_to(clip.llm_hook, 4),
        },
        "publicacion": meta.map(|m| json!({
            "title": m.title, "hook": m.hook, "description": m.description, "hashtags": m.hashtags,
        })),
        // Compacto a proposito: son miles de palabras y un diccionario por
        // palabra multiplica por cinco el tamano del fichero sin aportar nada.
        "palabras": clip.words.iter().map(|w| json!([w.text, round_to(w.start, 3), round_to(w.end, 3), round_to(w.probability, 3)])).collect::<Vec<_>>(),
    })
}

// ----------------------------------------------------------------------------
// Lectura
// ----------------------------------------------------------------------------

pub fn load(path: &Path) -> Result<Plan, PlanError> {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    if !path.is_file() {
        return Err(PlanError::NotFound(path.display().to_string()));
    }
    let text = std::fs::read_to_string(path).map_err(|e| PlanError::Io(format!("No se pudo leer {name}: {e}")))?;
    let data: Value = serde_json::from_str(&text).map_err(|e| PlanError::Invalid(format!("El plan {name} esta corrupto: {e}")))?;

    if data["version"].as_i64().unwrap_or(0) != FORMAT_VERSION {
        return Err(PlanError::Invalid(format!(
            "El plan {name} es de otra version de la aplicacion. Vuelve a buscar los momentos para generar uno nuevo."
        )));
    }

    let source = |key: &str| serde_json::from_value::<PlanSource>(data[key].clone()).unwrap_or_default();
    let clips = data["clips"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[])
        .iter()
        .map(|c| clip_from_value(c).ok_or_else(|| PlanError::Invalid(format!("El plan {name} esta corrupto: falta inicio o fin de un clip."))))
        .collect::<Result<Vec<_>, _>>()?;

    Ok(Plan {
        path: path.to_path_buf(),
        config: if data["config"].is_object() { data["config"].clone() } else { json!({}) },
        podcast: source("podcast"),
        background: source("background"),
        clips,
    })
}

fn clip_from_value(data: &Value) -> Option<PlanClip> {
    let publicacion = data["publicacion"].as_object().filter(|p| !p.is_empty());
    let text_of = |value: &Value| value.as_str().unwrap_or("").to_string();
    Some(PlanClip {
        start: data["inicio"].as_f64()?,
        end: data["fin"].as_f64()?,
        score: data["puntuacion"].as_f64().unwrap_or(0.0),
        reason: text_of(&data["motivo"]),
        text: text_of(&data["texto"]),
        selected: data["seleccionado"].as_bool().unwrap_or(true),
        signals: data["senales"].as_object().map(|m| m.iter().filter_map(|(k, v)| Some((k.clone(), v.as_f64()?))).collect()).unwrap_or_default(),
        meta: publicacion.map(|p| ClipMetadata {
            title: text_of(&p["title"]),
            hook: text_of(&p["hook"]),
            description: text_of(&p["description"]),
            hashtags: p["hashtags"].as_array().map(|t| t.iter().filter_map(Value::as_str).map(str::to_string).collect()).unwrap_or_default(),
        }),
        words: words_from_json(&data["palabras"]),
        style: text_of(&data["estilo"]),
        original: data["original"].as_object().and_then(|o| {
            Some(OriginalClip {
                start: o.get("inicio")?.as_f64()?,
                end: o.get("fin")?.as_f64()?,
                text: o.get("texto").and_then(Value::as_str).unwrap_or("").to_string(),
                words: words_from_json(o.get("palabras")?),
            })
        }),
    })
}

fn words_from_json(value: &Value) -> Vec<Word> {
    value
        .as_array()
        .map(|list| {
            list.iter()
                .filter_map(|w| {
                    Some(Word {
                        text: w[0].as_str()?.to_string(),
                        start: w[1].as_f64()?,
                        end: w[2].as_f64()?,
                        probability: w[3].as_f64().unwrap_or(1.0),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn words_to_json(words: &[Word]) -> Value {
    Value::Array(words.iter().map(|w| json!([w.text, round_to(w.start, 3), round_to(w.end, 3), round_to(w.probability, 3)])).collect())
}

fn join_words(words: &[Word]) -> String {
    words.iter().map(|w| w.text.as_str()).collect::<Vec<_>>().join(" ")
}

// ----------------------------------------------------------------------------
// Editar un clip: texto de los subtitulos, recorte y plantilla propia
// ----------------------------------------------------------------------------

/// Lleva el texto corregido por una persona a las palabras con tiempos, sin
/// perder los tiempos. Se alinean las palabras (sin mayusculas ni puntuacion)
/// y las que coinciden conservan su tiempo, pero toman el texto nuevo. Lo que se
/// ha cambiado se reparte por el hueco que ocupaba: una palabra por otra, dos por
/// tres, una de mas... Un texto vacio no borra los subtitulos.
pub fn retext_words(old: &[Word], text: &str) -> Vec<Word> {
    let new: Vec<&str> = text.split_whitespace().collect();
    if new.is_empty() || old.is_empty() {
        return old.to_vec();
    }
    let key = |s: &str| -> String { s.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect() };
    let old_keys: Vec<String> = old.iter().map(|w| key(&w.text)).collect();
    let new_keys: Vec<String> = new.iter().map(|w| key(w)).collect();
    let (n, m) = (old.len(), new.len());

    // Subsecuencia comun mas larga.
    let mut table = vec![vec![0usize; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            table[i][j] = if old_keys[i] == new_keys[j] { table[i + 1][j + 1] + 1 } else { table[i + 1][j].max(table[i][j + 1]) };
        }
    }
    let mut anchors: Vec<(usize, usize)> = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if old_keys[i] == new_keys[j] {
            anchors.push((i, j));
            i += 1;
            j += 1;
        } else if table[i + 1][j] >= table[i][j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    anchors.push((n, m)); // centinela: el hueco final

    let mut out: Vec<Word> = Vec::with_capacity(m);
    let (mut prev_i, mut prev_j) = (0, 0);
    for (ai, aj) in anchors {
        let gap_old = &old[prev_i..ai];
        let gap_new = &new[prev_j..aj];
        if !gap_new.is_empty() {
            let weights: Vec<f64> = gap_new.iter().map(|w| w.chars().count().max(1) as f64).collect();
            let total: f64 = weights.iter().sum();
            let (from, to) = if let (Some(first), Some(last)) = (gap_old.first(), gap_old.last()) {
                (first.start, last.end)
            } else {
                // Palabras anadidas sin nada que reemplazar: entre la anterior y la siguiente.
                let from = out.last().map_or_else(|| old[0].start, |w| w.end);
                let next_start = old.get(ai).map_or(from + 0.6, |w| w.start);
                let room = (next_start - from).max(0.0);
                let each = if room >= 0.05 * gap_new.len() as f64 { room / gap_new.len() as f64 } else { 0.12 };
                (from, from + each.min(0.3) * gap_new.len() as f64)
            };
            let mut cursor = from;
            for (word, weight) in gap_new.iter().zip(&weights) {
                let width = (to - from) * weight / total;
                out.push(Word { text: (*word).to_string(), start: cursor, end: cursor + width, probability: 1.0 });
                cursor += width;
            }
        }
        if ai < n {
            out.push(Word { text: new[aj].to_string(), start: old[ai].start, end: old[ai].end, probability: old[ai].probability });
        }
        prev_i = ai + 1;
        prev_j = aj + 1;
    }
    out
}

const STYLES: [&str; 5] = ["classic", "box", "marker", "neon", "minimal"];

/// Aplica los cambios de edicion de un clip sobre su JSON.
///
///  * `recorte`: `{inicio, fin}` = segundos que se quitan al principio y al final
///    del clip ORIGINAL (se puede volver a ampliar). Quedan al menos 5 s.
///  * `texto_palabras`: el texto de los subtitulos corregido.
///  * `estilo`: plantilla de subtitulos de este clip ("" = la de los ajustes).
///  * `restaurar`: vuelve al clip tal como salio de la seleccion.
fn edit_clip(clip: &mut Value, change: &Value) {
    let Some(mut c) = clip_from_value(clip) else { return };
    let touched = change.get("recorte").is_some() || change.get("texto_palabras").is_some() || change.get("restaurar").is_some() || change.get("estilo").is_some();
    if !touched {
        return;
    }
    let snapshot = |c: &PlanClip| OriginalClip { start: c.start, end: c.end, text: c.text.clone(), words: c.words.clone() };

    if change["restaurar"].as_bool() == Some(true) {
        if let Some(original) = c.original.take() {
            c.start = original.start;
            c.end = original.end;
            c.text = original.text;
            c.words = original.words;
        }
    }

    if let Some(trim) = change["recorte"].as_object() {
        let cut_start = trim.get("inicio").and_then(Value::as_f64).unwrap_or(0.0).clamp(0.0, 30.0);
        let cut_end = trim.get("fin").and_then(Value::as_f64).unwrap_or(0.0).clamp(0.0, 30.0);
        let original = c.original.clone().unwrap_or_else(|| snapshot(&c));
        let (now_start, now_end) = (c.start - original.start, original.end - c.end);
        // Solo si de verdad cambia: recalcular desde el original tiraria las
        // correcciones de texto hechas despues del ultimo recorte.
        let changed = (cut_start - now_start).abs() > 0.01 || (cut_end - now_end).abs() > 0.01;
        let (new_start, new_end) = (original.start + cut_start, original.end - cut_end);
        if changed && new_end - new_start >= 5.0 {
            let words: Vec<Word> = original.words.iter().filter(|w| w.start >= new_start - 0.02 && w.end <= new_end + 0.02).cloned().collect();
            if !words.is_empty() {
                c.start = new_start;
                c.end = new_end;
                c.text = join_words(&words);
                c.words = words;
                c.original = Some(original);
                if cut_start == 0.0 && cut_end == 0.0 && c.original.as_ref().is_some_and(|o| o.words == c.words) {
                    c.original = None; // vuelve a ser el clip sin tocar
                }
            }
        }
    }

    if let Some(text) = change["texto_palabras"].as_str() {
        let edited = retext_words(&c.words, text);
        if edited != c.words {
            if c.original.is_none() {
                c.original = Some(snapshot(&c));
            }
            c.text = join_words(&edited);
            c.words = edited;
        }
    }

    if let Some(style) = change["estilo"].as_str() {
        if style.is_empty() || STYLES.contains(&style) {
            c.style = style.to_string();
        }
    }

    clip["inicio"] = json!(round_to(c.start, 3));
    clip["fin"] = json!(round_to(c.end, 3));
    clip["marca"] = json!(hms(c.start));
    clip["duracion"] = json!(round_to(c.duration(), 1));
    clip["texto"] = json!(c.text);
    clip["palabras"] = words_to_json(&c.words);
    clip["estilo"] = json!(c.style);
    match &c.original {
        Some(o) => clip["original"] = json!({"inicio": round_to(o.start, 3), "fin": round_to(o.end, 3), "texto": o.text, "palabras": words_to_json(&o.words)}),
        None => {
            if let Some(map) = clip.as_object_mut() {
                map.remove("original");
            }
        }
    }
}

/// Aplica los cambios de la interfaz (marcar, titular) sobre el fichero.
///
/// `changes` trae una entrada por clip (`n` desde 1) con las claves que se
/// hayan tocado: `seleccionado` y `publicacion` (title/hook/description/hashtags).
pub fn update(path: &Path, changes: &[Value]) -> Result<Plan, PlanError> {
    let mut data = read_raw(path)?;
    if let Some(clips) = data["clips"].as_array_mut() {
        for change in changes {
            let Some(index) = change["n"].as_i64().map(|n| n - 1).filter(|n| (0..clips.len() as i64).contains(n)) else { continue };
            let clip = &mut clips[index as usize];
            edit_clip(clip, change);
            if let Some(selected) = change["seleccionado"].as_bool() {
                clip["seleccionado"] = Value::Bool(selected);
            }
            if let Some(publicacion) = change["publicacion"].as_object().filter(|p| !p.is_empty()) {
                let mut actual = match clip["publicacion"].as_object() {
                    Some(existing) => existing.clone(),
                    None => json!({"title": "", "hook": "", "description": "", "hashtags": []}).as_object().cloned().unwrap_or_default(),
                };
                for key in ["title", "hook", "description"] {
                    if let Some(value) = publicacion.get(key) {
                        actual.insert(key.into(), Value::String(as_text(value).trim().to_string()));
                    }
                }
                if let Some(tags) = publicacion.get("hashtags").and_then(Value::as_array) {
                    let cleaned: Vec<Value> = tags
                        .iter()
                        .map(|t| as_text(t).trim_start_matches('#').trim().to_string())
                        .filter(|t| !t.is_empty())
                        .map(Value::String)
                        .collect();
                    actual.insert("hashtags".into(), Value::Array(cleaned));
                }
                clip["publicacion"] = Value::Object(actual);
            }
        }
    }
    write_json(path, &data)?;
    load(path)
}

/// Cambia ajustes del plan ("subtitles.font_size": 80) antes de renderizar.
pub fn apply_config(path: &Path, overrides: &Map<String, Value>) -> Result<(), PlanError> {
    let mut data = read_raw(path)?;
    if !data["config"].is_object() {
        data["config"] = json!({});
    }
    for (dotted, value) in overrides {
        let parts: Vec<&str> = dotted.split('.').collect();
        let mut target = &mut data["config"];
        for part in &parts[..parts.len() - 1] {
            if !target.is_object() {
                return Err(PlanError::Invalid(format!("'{dotted}' no es un ajuste valido.")));
            }
            target = target.as_object_mut().expect("comprobado").entry(part.to_string()).or_insert_with(|| json!({}));
        }
        let Some(map) = target.as_object_mut() else {
            return Err(PlanError::Invalid(format!("'{dotted}' no es un ajuste valido.")));
        };
        map.insert(parts[parts.len() - 1].to_string(), value.clone());
    }
    write_json(path, &data)
}

fn read_raw(path: &Path) -> Result<Value, PlanError> {
    let text = std::fs::read_to_string(path).map_err(|e| PlanError::Io(format!("No se pudo leer {}: {e}", path.display())))?;
    serde_json::from_str(&text).map_err(|e| PlanError::Invalid(format!("El plan {} esta corrupto: {e}", path.display())))
}

fn as_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fuente(ruta: &str) -> PlanSource {
        PlanSource { ruta: ruta.into(), titulo: "El Programa - Invitado".into(), duracion: 3600.0, url: "https://example.com/v".into() }
    }

    fn clip(inicio: f64) -> Window {
        let palabras = vec![Word::new("hola", inicio, inicio + 0.4), Word::new("mundo.", inicio + 0.5, inicio + 0.9)];
        Window { score: 0.7, reason: "engancha".into(), heat: 0.5, ..Window::new(inicio, inicio + 30.0, palabras, "hola mundo.") }
    }

    fn dichas(textos: &[&str]) -> Vec<Word> {
        textos.iter().enumerate().map(|(i, t)| Word::new(*t, i as f64, i as f64 + 0.8)).collect()
    }

    fn texto(words: &[Word]) -> String {
        words.iter().map(|w| w.text.as_str()).collect::<Vec<_>>().join(" ")
    }

    #[test]
    fn corregir_una_palabra_conserva_los_tiempos() {
        let antes = dichas(&["cuando", "compártelo", "tiempo", "y", "espacios"]);
        let despues = retext_words(&antes, "cuando compartes tiempo y espacios");
        assert_eq!(texto(&despues), "cuando compartes tiempo y espacios");
        for (a, d) in antes.iter().zip(&despues) {
            assert_eq!((a.start, a.end), (d.start, d.end));
        }
        // Mayusculas y puntuacion tambien se corrigen sin tocar el tiempo.
        let con_puntos = retext_words(&antes, "Cuando compartes tiempo, y espacios.");
        assert_eq!(texto(&con_puntos), "Cuando compartes tiempo, y espacios.");
        assert_eq!(con_puntos[1].start, antes[1].start);
    }

    #[test]
    fn cambiar_dos_palabras_por_tres_reparte_el_hueco() {
        let antes = dichas(&["uno", "dos", "tres", "cuatro"]);
        let despues = retext_words(&antes, "uno cinco seis siete cuatro");
        assert_eq!(texto(&despues), "uno cinco seis siete cuatro");
        // Las nuevas ocupan justo lo que ocupaban "dos tres" (de 1.0 a 2.8).
        assert!((despues[1].start - 1.0).abs() < 1e-9 && (despues[3].end - 2.8).abs() < 1e-9);
        assert!(despues.windows(2).all(|p| p[0].end <= p[1].start + 1e-9), "sin solapes ni vueltas atras");
        assert_eq!((despues[0].start, despues[4].start), (0.0, 3.0), "las que no se tocan, igual");
    }

    #[test]
    fn quitar_y_anadir_palabras() {
        let antes = dichas(&["a", "b", "c", "d"]);
        assert_eq!(texto(&retext_words(&antes, "a d")), "a d");
        let mas = retext_words(&antes, "a b nueva c d");
        assert_eq!(texto(&mas), "a b nueva c d");
        let nueva = &mas[2];
        assert!(nueva.start >= antes[1].end - 1e-9 && nueva.end <= antes[2].start + 1e-9 + 0.3, "la nueva cabe entre b y c: {nueva:?}");
        assert_eq!(texto(&retext_words(&antes, "   ")), "a b c d", "un texto vacio no borra los subtitulos");
        assert_eq!(texto(&retext_words(&[], "hola")), "", "sin palabras no hay donde poner tiempos");
    }

    fn plan_con_clip_largo(dir: &Path) -> PathBuf {
        let palabras: Vec<Word> = (0..40).map(|i| Word::new(format!("p{i}"), 100.0 + i as f64, 100.0 + i as f64 + 0.8)).collect();
        let largo = Window { score: 0.7, ..Window::new(100.0, 140.0, palabras, "texto") };
        save(&dir.join("plan.json"), &Config::default(), &fuente("p.mp4"), &fuente("b.mp4"), &[largo], &HashMap::new(), 1).unwrap()
    }

    #[test]
    fn recortar_corregir_y_restaurar_un_clip() {
        let dir = tempfile::tempdir().unwrap();
        let ruta = plan_con_clip_largo(dir.path());

        // Recorte: 5 s al principio y 3 al final, desde el original.
        let plan = update(&ruta, &[json!({"n": 1, "recorte": {"inicio": 5.0, "fin": 3.0}})]).unwrap();
        let clip = &plan.clips[0];
        assert_eq!((clip.start, clip.end), (105.0, 137.0));
        assert_eq!(clip.words.first().unwrap().text, "p5");
        assert_eq!(clip.words.last().unwrap().text, "p36");
        assert!(clip.original.is_some());

        // Corregir texto sobre el recortado; repetir el mismo recorte no lo pierde.
        let editado = texto(&clip.words).replacen("p5 p6", "cinco seis", 1);
        update(&ruta, &[json!({"n": 1, "texto_palabras": editado})]).unwrap();
        let plan = update(&ruta, &[json!({"n": 1, "recorte": {"inicio": 5.0, "fin": 3.0}})]).unwrap();
        assert_eq!(plan.clips[0].words[0].text, "cinco", "el mismo recorte no recalcula desde el original");

        // Volver a ampliar el recorte lee otra vez del original (se pierde el texto).
        let plan = update(&ruta, &[json!({"n": 1, "recorte": {"inicio": 2.0, "fin": 0.0}})]).unwrap();
        assert_eq!((plan.clips[0].start, plan.clips[0].end), (102.0, 140.0));
        assert_eq!(plan.clips[0].words[0].text, "p2");

        // Un recorte que deja menos de 5 s se ignora.
        let plan = update(&ruta, &[json!({"n": 1, "recorte": {"inicio": 30.0, "fin": 30.0}})]).unwrap();
        assert_eq!(plan.clips[0].end - plan.clips[0].start, 38.0);

        // Restaurar: igual que al salir de la seleccion.
        let plan = update(&ruta, &[json!({"n": 1, "restaurar": true})]).unwrap();
        let clip = &plan.clips[0];
        assert_eq!((clip.start, clip.end, clip.words.len()), (100.0, 140.0, 40));
        assert!(clip.original.is_none());
    }

    #[test]
    fn la_plantilla_propia_de_un_clip_solo_admite_las_conocidas() {
        let dir = tempfile::tempdir().unwrap();
        let ruta = plan_con_clip_largo(dir.path());
        assert_eq!(update(&ruta, &[json!({"n": 1, "estilo": "neon"})]).unwrap().clips[0].style, "neon");
        assert_eq!(update(&ruta, &[json!({"n": 1, "estilo": "inventado"})]).unwrap().clips[0].style, "neon", "una plantilla desconocida no cambia nada");
        assert_eq!(update(&ruta, &[json!({"n": 1, "estilo": ""})]).unwrap().clips[0].style, "");
        // Y el render la recibe en la ventana.
        let plan = update(&ruta, &[json!({"n": 1, "estilo": "marker"})]).unwrap();
        assert_eq!(plan.clips[0].to_window().style, "marker");
    }

    #[test]
    fn ida_y_vuelta_conserva_lo_que_importa() {
        let dir = tempfile::tempdir().unwrap();
        let destino = dir.path().join("plan.json");
        let mut cfg = Config::default();
        cfg.subtitles.font_size = 77;
        let metas = HashMap::from([(0, ClipMetadata::new("Titulo", "GANCHO", "Descripcion.", &["a", "b"]))]);

        save(&destino, &cfg, &fuente("p.mp4"), &fuente("b.mp4"), &[clip(10.0), clip(200.0)], &metas, 1).unwrap();
        let leido = load(&destino).unwrap();

        assert_eq!(leido.clips.len(), 2);
        assert_eq!(leido.selected(), [0]);
        assert_eq!(leido.config["subtitles"]["font_size"], 77);
        assert_eq!(leido.clips[0].meta.as_ref().unwrap().title, "Titulo");
        assert!(leido.clips[1].meta.is_none());
        assert_eq!(leido.clips[0].words.len(), 2);
        assert_eq!(leido.podcast.titulo, "El Programa - Invitado");

        let ventana = leido.clips[0].to_window();
        assert!((ventana.start - 10.0).abs() < 1e-9 && (ventana.heat - 0.5).abs() < 1e-9);
        assert_eq!(ventana.words[0].text, "hola");
        assert_eq!(ventana.reason, "engancha");
        assert_eq!(leido.clips[0].duration(), 30.0);
    }

    #[test]
    fn la_configuracion_del_plan_se_vuelca_sobre_un_config() {
        let dir = tempfile::tempdir().unwrap();
        let destino = dir.path().join("plan.json");
        let mut cfg = Config::default();
        cfg.clips.num_clips = 9;
        save(&destino, &cfg, &fuente("p"), &fuente("b"), &[clip(1.0)], &HashMap::new(), 1).unwrap();

        let mut recuperada = Config::default();
        recuperada.merge(&load(&destino).unwrap().config, "plan").unwrap();
        assert_eq!(recuperada, cfg);
    }

    #[test]
    fn la_interfaz_puede_marcar_y_retitular() {
        let dir = tempfile::tempdir().unwrap();
        let destino = dir.path().join("plan.json");
        save(&destino, &Config::default(), &fuente("p"), &fuente("b"), &[clip(10.0), clip(200.0)], &HashMap::new(), 2).unwrap();

        update(
            &destino,
            &[
                json!({"n": 1, "seleccionado": false}),
                json!({"n": 2, "publicacion": {"title": " Otro titulo ", "hashtags": ["#uno", "dos", " "]}}),
                json!({"n": 99, "seleccionado": false}),
                json!({"n": 0}),
            ],
        )
        .unwrap();
        let leido = load(&destino).unwrap();
        assert_eq!(leido.selected(), [1]);
        let meta = leido.clips[1].meta.as_ref().unwrap();
        assert_eq!((meta.title.as_str(), meta.hashtags.clone()), ("Otro titulo", vec!["uno".to_string(), "dos".to_string()]));
        assert_eq!(meta.hook, "", "lo que no se toca queda como estaba");

        let mut cambios = Map::new();
        cambios.insert("subtitles.font_size".into(), json!(120));
        cambios.insert("zoom.enabled".into(), json!(false));
        apply_config(&destino, &cambios).unwrap();
        let config = load(&destino).unwrap().config;
        assert_eq!((&config["subtitles"]["font_size"], &config["zoom"]["enabled"]), (&json!(120), &json!(false)));
    }

    #[test]
    fn un_ajuste_que_no_es_un_grupo_es_un_error() {
        let dir = tempfile::tempdir().unwrap();
        let destino = dir.path().join("plan.json");
        save(&destino, &Config::default(), &fuente("p"), &fuente("b"), &[clip(1.0)], &HashMap::new(), 1).unwrap();
        let mut cambios = Map::new();
        cambios.insert("clips.num_clips.dentro".into(), json!(1));
        assert!(apply_config(&destino, &cambios).is_err());
    }

    #[test]
    fn un_plan_de_otra_version_o_roto_no_se_usa_a_medias() {
        let dir = tempfile::tempdir().unwrap();
        let destino = dir.path().join("plan.json");
        std::fs::write(&destino, r#"{"version": 0, "clips": []}"#).unwrap();
        assert!(load(&destino).unwrap_err().to_string().contains("otra version"));
        std::fs::write(&destino, "{roto").unwrap();
        assert!(load(&destino).unwrap_err().to_string().contains("corrupto"));
        assert!(matches!(load(&dir.path().join("no_existe.json")), Err(PlanError::NotFound(_))));
        std::fs::write(&destino, json!({"version": 2, "clips": [{"fin": 3}]}).to_string()).unwrap();
        assert!(load(&destino).is_err());
    }

    #[test]
    fn se_leen_los_planes_que_escribio_la_version_python() {
        let python = json!({
            "version": 2, "creado": "2026-10-18 20:00:00",
            "config": {"clips": {"num_clips": 4}},
            "podcast": {"ruta": "C:/p.mp4", "titulo": "P", "duracion": 100.0, "url": ""},
            "background": {"ruta": "C:/b.mp4", "titulo": "B", "duracion": 50.0, "url": ""},
            "clips": [{"inicio": 10.0, "fin": 40.0, "marca": "0:10", "duracion": 30.0, "puntuacion": 0.5, "motivo": "x",
                       "texto": "t", "seleccionado": true, "senales": {"heat": 0.2}, "publicacion": null,
                       "palabras": [["hola", 10.0, 10.4, 0.9], ["mundo", 10.5, 10.9]]}]
        });
        let dir = tempfile::tempdir().unwrap();
        let destino = dir.path().join("plan.json");
        std::fs::write(&destino, python.to_string()).unwrap();
        let plan = load(&destino).unwrap();
        assert_eq!(plan.clips[0].words[1].probability, 1.0, "la probabilidad es opcional");
        assert!(plan.clips[0].meta.is_none());
        assert_eq!(plan.podcast.ruta, "C:/p.mp4");
        assert_eq!(plan.clips[0].signals["heat"], 0.2);
    }

    #[test]
    fn el_redondeo_es_el_de_python() {
        assert_eq!(round_to(0.5, 0), 0.0);
        assert_eq!(round_to(1.5, 0), 2.0);
        assert_eq!(round_to(2.5, 0), 2.0);
        assert_eq!(round_to(10.0005, 3), 10.0);
    }
}
