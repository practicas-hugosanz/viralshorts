//! Seleccion de los momentos del podcast.
//!
//! El criterio original (densidad de palabras) premia a quien habla mas rapido,
//! no al momento que engancha. Aqui se combinan cuatro senales independientes:
//!
//!   1. heatmap  - el "most replayed" de YouTube: la audiencia diciendo que
//!                 parte rebobina. La senal individual mas fuerte.
//!   2. energia  - risas, subidas de voz, discusiones (ver `audio_energy`).
//!   3. densidad - palabras por segundo. Peso bajo: descarta silencios largos.
//!   4. Claude   - lee la transcripcion y puntua contenido y gancho.
//!
//! Las ventanas candidatas se generan SIEMPRE cortando en frontera de frase,
//! de modo que ningun clip empieza ni acaba a media palabra.

use std::collections::HashMap;

use crate::audio_energy::EnergyTrack;
use crate::config::{ClipsConfig, ScoringConfig};
use crate::download::HeatPoint;
use crate::prescan::heat_for;
use crate::words::{Transcript, Word};

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct SelectionError(pub String);

/// Un tramo candidato, alineado a frontera de frase.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Window {
    pub start: f64,
    pub end: f64,
    pub words: Vec<Word>,
    pub text: String,

    pub heat: f64,
    pub energy: f64,
    pub density: f64,
    pub context: f64,
    pub llm_content: f64,
    pub llm_hook: f64,
    pub reason: String,
    pub score: f64,
    /// Plantilla de subtitulos propia de este clip (vacio = la de la configuracion).
    pub style: String,
}

impl Window {
    pub fn new(start: f64, end: f64, words: Vec<Word>, text: impl Into<String>) -> Self {
        Self { start, end, words, text: text.into(), ..Default::default() }
    }

    pub fn duration(&self) -> f64 {
        self.end - self.start
    }

    pub fn overlaps(&self, other: &Window, gap: f64) -> bool {
        self.start < other.end + gap && self.end > other.start - gap
    }

    pub fn describe(&self) -> String {
        let mut parts = vec![
            format!("{}-{} ({:.0}s)", hms(self.start), hms(self.end), self.duration()),
            format!("total {:.3}", self.score),
            format!("heat {:.2}", self.heat),
            format!("energia {:.2}", self.energy),
            format!("contexto {:.2}", self.context),
        ];
        if self.llm_content != 0.0 || self.llm_hook != 0.0 {
            parts.push(format!("contenido {:.2}", self.llm_content));
            parts.push(format!("gancho {:.2}", self.llm_hook));
        }
        parts.join(" | ")
    }

    fn signal(&self, name: &str) -> f64 {
        match name {
            "heat" => self.heat,
            "energy" => self.energy,
            "density" => self.density,
            "context" => self.context,
            "llm_content" => self.llm_content,
            "llm_hook" => self.llm_hook,
            other => panic!("senal desconocida: {other}"),
        }
    }
}

/// Nota de Claude para un fragmento, ya en 0..1.
#[derive(Debug, Clone, PartialEq)]
pub struct LlmScore {
    pub content: f64,
    pub hook: f64,
    pub reason: String,
}

// ----------------------------------------------------------------------------
// 1. Generacion de candidatos
// ----------------------------------------------------------------------------

/// Genera ventanas candidatas cortando en frontera de frase.
///
/// Los puntos de corte son los finales de frase (y, como respaldo, los finales
/// de segmento), de modo que un clip nunca arranca ni acaba a mitad de una
/// palabra. Si se transcribio por zonas (ver `prescan`), `regions` marca los
/// tramos con texto: una ventana que empezara en una zona y acabara en otra se
/// saltaria todo el hueco intermedio, asi que se descartan.
pub fn build_windows(
    transcript: &Transcript,
    cfg: &ClipsConfig,
    regions: Option<&[(f64, f64)]>,
) -> Result<Vec<Window>, SelectionError> {
    let boundaries = sentence_boundaries(transcript);
    if boundaries.len() < 2 {
        return Err(SelectionError("La transcripcion no tiene suficientes frases para cortar clips.".into()));
    }

    let words = transcript.words();
    let midpoints: Vec<f64> = words.iter().map(|w| (w.start + w.end) / 2.0).collect();
    let bisect_left = |value: f64| midpoints.partition_point(|&m| m < value);

    let mut usable_start = cfg.head_skip;
    let mut usable_end = transcript.duration - cfg.tail_skip;
    if usable_end - usable_start < cfg.min_duration {
        // Video mas corto que los margenes: recortar la intro y la despedida
        // dejaria cero candidatos, asi que se ignoran los margenes.
        log::warn!(
            "El video dura {:.0}s y los margenes ({:.0}s + {:.0}s) no dejan sitio para un clip de {:.0}s; se ignoran los margenes.",
            transcript.duration, cfg.head_skip, cfg.tail_skip, cfg.min_duration
        );
        usable_start = 0.0;
        usable_end = transcript.duration;
    }

    let region_of = region_index(regions);

    let mut windows = Vec::new();
    for (i, &start) in boundaries[..boundaries.len() - 1].iter().enumerate() {
        if start < usable_start || start > usable_end {
            continue;
        }
        let Some(start_region) = region_of(start) else { continue };
        let lo = bisect_left(start);
        for &end in &boundaries[i + 1..] {
            let length = end - start;
            if length < cfg.min_duration {
                continue;
            }
            if length > cfg.max_duration || end > usable_end {
                break;
            }
            if region_of(end) != Some(start_region) {
                break;
            }
            let hi = bisect_left(end);
            let chunk = &words[lo..hi.max(lo)];
            if chunk.len() < 10 {
                continue;
            }
            let text = chunk.iter().map(|w| w.text.trim()).collect::<Vec<_>>().join(" ").trim().to_string();
            windows.push(Window::new(start, end, chunk.to_vec(), text));
        }
    }

    log::info!(
        "Generadas {} ventanas candidatas entre {} y {}.",
        windows.len(),
        hms(usable_start),
        hms(usable_end)
    );
    if windows.is_empty() {
        return Err(SelectionError(
            "No se pudo generar ninguna ventana candidata. Prueba a bajar clips.min_duration o \
             clips.head_skip, o a subir clips.max_duration."
                .into(),
        ));
    }
    Ok(windows)
}

/// Funcion instante -> indice de zona (None si esta fuera de todas).
fn region_index(regions: Option<&[(f64, f64)]>) -> Box<dyn Fn(f64) -> Option<usize> + '_> {
    let Some(regions) = regions.filter(|r| !r.is_empty()) else {
        return Box::new(|_| Some(0));
    };
    let mut ordered: Vec<(f64, f64)> = regions.to_vec();
    ordered.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    Box::new(move |moment| {
        let upper = ordered.partition_point(|r| r.0 <= moment); // bisect_right sobre los inicios
        let i = upper.checked_sub(1)?;
        (moment <= ordered[i].1).then_some(i)
    })
}

fn sentence_boundaries(transcript: &Transcript) -> Vec<f64> {
    let mut points = vec![0.0];
    for segment in &transcript.segments {
        points.push(segment.end);
        points.extend(segment.words.iter().filter(|w| w.ends_sentence()).map(|w| w.end));
    }
    points.sort_by(|a, b| a.total_cmp(b));
    points.dedup();
    points
}

// ----------------------------------------------------------------------------
// 2. Senales baratas
// ----------------------------------------------------------------------------

/// Rellena heat / energy / density en cada ventana.
pub fn apply_cheap_signals(windows: &mut [Window], heatmap: &[HeatPoint], energy: Option<&EnergyTrack>) {
    for window in windows {
        window.heat = heat_for(heatmap, window.start, window.end);
        window.energy = energy.map_or(0.0, |e| e.window_score(window.start, window.end));
        window.density = window.words.len() as f64 / window.duration().max(1e-6);
    }
}

// ----------------------------------------------------------------------------
// 3. Combinacion y seleccion
// ----------------------------------------------------------------------------

/// Mejores ventanas por senales baratas, sin solaparse entre si. Sin el filtro
/// de solape se mandarian al LLM veinte versiones del mismo momento
/// desplazadas dos segundos, que es tirar el dinero.
pub fn shortlist_for_llm(windows: &[Window], limit: usize, cfg: &ScoringConfig) -> Vec<Window> {
    let weights = [
        ("heat", cfg.weight_heatmap),
        ("energy", cfg.weight_energy),
        ("density", cfg.weight_density),
        ("context", cfg.weight_context),
    ];
    let ranges = compute_ranges(windows, &weights);
    let mut ranked: Vec<(f64, &Window)> = windows.iter().map(|w| (weighted(w, &weights, &ranges), w)).collect();
    ranked.sort_by(|a, b| b.0.total_cmp(&a.0));

    let mut picked: Vec<Window> = Vec::new();
    for (_, window) in ranked {
        if picked.len() >= limit {
            break;
        }
        if picked.iter().any(|other| window.overlaps(other, 0.0)) {
            continue;
        }
        picked.push(window.clone());
    }
    picked.sort_by(|a, b| a.start.total_cmp(&b.start));
    picked
}

pub fn apply_llm_scores(windows: &mut [Window], scores: &HashMap<usize, LlmScore>) {
    for (&index, score) in scores {
        if let Some(window) = windows.get_mut(index) {
            window.llm_content = score.content;
            window.llm_hook = score.hook;
            window.reason = score.reason.clone();
        }
    }
}

/// Puntua definitivamente y elige los mejores clips sin solaparse. `limit`
/// manda sobre `clips.num_clips`: al preparar una seleccion para revisar a
/// mano interesa proponer mas candidatos de los que se van a renderizar.
pub fn finalise(windows: &mut [Window], scoring: &ScoringConfig, clips: &ClipsConfig, limit: Option<usize>) -> Vec<Window> {
    let weights = [
        ("heat", scoring.weight_heatmap),
        ("energy", scoring.weight_energy),
        ("density", scoring.weight_density),
        ("context", scoring.weight_context),
        ("llm_content", scoring.weight_llm),
        ("llm_hook", scoring.weight_hook),
    ];
    let ranges = compute_ranges(windows, &weights);
    for window in windows.iter_mut() {
        window.score = weighted(window, &weights, &ranges);
    }

    let wanted = limit.unwrap_or(clips.num_clips.max(0) as usize);
    let max_similarity = scoring.max_similarity;

    let mut order: Vec<&Window> = windows.iter().collect();
    order.sort_by(|a, b| b.score.total_cmp(&a.score));

    let mut picked: Vec<Window> = Vec::new();
    let mut fingerprints: Vec<HashMap<String, f64>> = Vec::new();
    for window in order {
        if picked.len() >= wanted {
            break;
        }
        if picked.iter().any(|other| window.overlaps(other, clips.min_gap)) {
            continue;
        }
        // Dos clips del mismo tema, aunque esten en minutos distintos, son el
        // mismo video contado dos veces: el segundo no aporta nada al feed.
        if max_similarity < 1.0 {
            let fingerprint = fingerprint(&window.text);
            if let Some(twin) = fingerprints.iter().position(|other| cosine(&fingerprint, other) > max_similarity) {
                log::debug!("Descartado {} por repetir el tema de {}.", hms(window.start), hms(picked[twin].start));
                continue;
            }
            fingerprints.push(fingerprint);
        }
        picked.push(window.clone());
    }

    picked.sort_by(|a, b| a.start.total_cmp(&b.start));
    picked
}

// ----------------------------------------------------------------------------
// Parecido tematico entre dos clips
// ----------------------------------------------------------------------------

/// Palabras que aparecen en todos los clips y no dicen de que va ninguno.
const STOPWORDS: &str = "
que para pero como cuando donde porque entonces tambien aunque desde hasta
sobre entre todo toda todos todas nada nadie mucho mucha muy mas menos entre
entonces claro bueno vale digo dice decir hacer hecho tiene tienen tener
estaba estan estoy esta este esta esto eso esa ese esos esas aqui alli ahora
siempre nunca luego despues antes ellos ellas nosotros vosotros usted ustedes
";

fn strip_accents(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            'á' | 'Á' => 'a',
            'é' | 'É' => 'e',
            'í' | 'Í' => 'i',
            'ó' | 'Ó' => 'o',
            'ú' | 'ü' | 'Ú' | 'Ü' => 'u',
            other => other,
        })
        .collect()
}

/// Vocabulario con peso de un clip, sin muletillas ni palabras vacias.
fn fingerprint(text: &str) -> HashMap<String, f64> {
    let folded = strip_accents(&text.to_lowercase());
    let mut counts: HashMap<String, f64> = HashMap::new();
    for word in folded.split(|c: char| !(c.is_alphanumeric() || c == '_')).filter(|w| w.chars().count() >= 4) {
        if !STOPWORDS.split_whitespace().any(|s| s == word) {
            *counts.entry(word.to_string()).or_default() += 1.0;
        }
    }
    counts
}

fn cosine(a: &HashMap<String, f64>, b: &HashMap<String, f64>) -> f64 {
    let dot: f64 = a.iter().filter_map(|(word, &x)| b.get(word).map(|&y| x * y)).sum();
    if dot == 0.0 {
        return 0.0;
    }
    let norm = |m: &HashMap<String, f64>| m.values().map(|v| v * v).sum::<f64>().sqrt();
    let (na, nb) = (norm(a), norm(b));
    if na > 0.0 && nb > 0.0 { dot / (na * nb) } else { 0.0 }
}

type Ranges<'a> = HashMap<&'a str, (f64, f64)>;

fn compute_ranges<'a>(population: &[Window], weights: &[(&'a str, f64)]) -> Ranges<'a> {
    weights
        .iter()
        .map(|&(name, _)| {
            let values = population.iter().map(|w| w.signal(name));
            let range = values.clone().fold(None, |acc: Option<(f64, f64)>, v| match acc {
                None => Some((v, v)),
                Some((lo, hi)) => Some((lo.min(v), hi.max(v))),
            });
            (name, range.unwrap_or((0.0, 0.0)))
        })
        .collect()
}

/// Suma ponderada de las senales, cada una normalizada 0..1 en la poblacion.
/// Normalizar por poblacion evita que una senal con rango pequeno (el heatmap
/// de un video muy plano) domine o desaparezca por accidente. Una senal
/// constante -- porque no hay heatmap, o porque el LLM no respondio -- no
/// aporta informacion: se ignora y su peso se reparte entre las demas.
fn weighted(window: &Window, weights: &[(&str, f64)], ranges: &Ranges) -> f64 {
    let (mut total, mut total_weight) = (0.0, 0.0);
    for &(name, weight) in weights {
        if weight <= 0.0 {
            continue;
        }
        let (low, high) = ranges[name];
        if high - low < 1e-9 {
            continue;
        }
        total += weight * (window.signal(name) - low) / (high - low);
        total_weight += weight;
    }
    if total_weight > 0.0 { total / total_weight } else { 0.0 }
}

pub fn hms(seconds: f64) -> String {
    let total = seconds as i64;
    let (minutes, secs) = (total / 60, total % 60);
    let (hours, minutes) = (minutes / 60, minutes % 60);
    if hours > 0 { format!("{hours}:{minutes:02}:{secs:02}") } else { format!("{minutes}:{secs:02}") }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::words::Segment;

    /// Un segmento que termina en punto, como una frase de verdad.
    fn frase(inicio: f64, palabras: &str) -> Segment {
        let paso = 0.4;
        let trozos: Vec<&str> = palabras.split_whitespace().collect();
        let mut lista = Vec::new();
        let mut momento = inicio;
        for (i, texto) in trozos.iter().enumerate() {
            let texto = if i == trozos.len() - 1 { format!("{texto}.") } else { texto.to_string() };
            lista.push(Word::new(texto, momento, momento + paso * 0.8));
            momento += paso;
        }
        Segment { start: inicio, end: momento, text: palabras.into(), words: lista }
    }

    fn transcripcion(duracion: f64, tema: &str) -> Transcript {
        let mut segmentos = Vec::new();
        let mut momento = 0.0;
        while momento < duracion - 12.0 {
            segmentos.push(frase(momento, &format!("hablamos de {tema} en el minuto {}", momento as i64)));
            momento += 12.0;
        }
        Transcript { language: "es".into(), duration: duracion, segments: segmentos }
    }

    fn tema() -> &'static str {
        "bitcoin dinero banco inversion"
    }

    // -- candidatos ----------------------------------------------------------

    #[test]
    fn las_ventanas_respetan_duracion_y_margenes() {
        let cfg = ClipsConfig { min_duration: 25.0, max_duration: 45.0, head_skip: 60.0, tail_skip: 30.0, ..Default::default() };
        let ventanas = build_windows(&transcripcion(600.0, tema()), &cfg, None).unwrap();
        assert!(!ventanas.is_empty());
        for v in &ventanas {
            assert!(v.duration() >= cfg.min_duration - 1e-6 && v.duration() <= cfg.max_duration + 1e-6);
            assert!(v.start >= cfg.head_skip && v.end <= 600.0 - cfg.tail_skip);
        }
    }

    #[test]
    fn un_video_mas_corto_que_los_margenes_no_se_queda_sin_clips() {
        let cfg = ClipsConfig { min_duration: 20.0, max_duration: 40.0, head_skip: 300.0, tail_skip: 300.0, ..Default::default() };
        assert!(!build_windows(&transcripcion(200.0, tema()), &cfg, None).unwrap().is_empty());
    }

    #[test]
    fn las_ventanas_no_cruzan_de_una_zona_a_otra() {
        let cfg = ClipsConfig { min_duration: 20.0, max_duration: 60.0, head_skip: 0.0, tail_skip: 0.0, ..Default::default() };
        let zonas = [(0.0, 100.0), (400.0, 500.0)];
        for v in build_windows(&transcripcion(600.0, tema()), &cfg, Some(&zonas)).unwrap() {
            assert!(zonas.iter().any(|&(a, b)| a <= v.start && v.end <= b), "{}-{} se sale de las zonas", v.start, v.end);
        }
    }

    #[test]
    fn sin_frases_suficientes_o_sin_ventanas_es_un_error_claro() {
        let vacia = Transcript { language: "es".into(), duration: 100.0, segments: vec![] };
        assert!(build_windows(&vacia, &ClipsConfig::default(), None).is_err());
        let cfg = ClipsConfig { min_duration: 700.0, max_duration: 800.0, head_skip: 0.0, tail_skip: 0.0, ..Default::default() };
        let error = build_windows(&transcripcion(600.0, tema()), &cfg, None).unwrap_err().to_string();
        assert!(error.contains("min_duration"), "{error}");
    }

    #[test]
    fn las_ventanas_empiezan_y_acaban_en_final_de_frase() {
        let cfg = ClipsConfig { min_duration: 25.0, max_duration: 45.0, head_skip: 0.0, tail_skip: 0.0, ..Default::default() };
        let t = transcripcion(600.0, tema());
        let mut finales: Vec<f64> = t.segments.iter().flat_map(|s| s.words.iter().filter(|w| w.ends_sentence()).map(|w| w.end)).collect();
        finales.extend(t.segments.iter().map(|s| s.end));
        for v in build_windows(&t, &cfg, None).unwrap() {
            assert!(v.start == 0.0 || finales.iter().any(|&f| (f - v.start).abs() < 1e-9), "empieza a mitad de frase: {}", v.start);
            assert!(finales.iter().any(|&f| (f - v.end).abs() < 1e-9), "acaba a mitad de frase: {}", v.end);
        }
    }

    // -- eleccion ------------------------------------------------------------

    fn ventana(inicio: f64, texto: &str, nota: f64) -> Window {
        let mut v = Window::new(inicio, inicio + 30.0, vec![], texto);
        v.llm_content = nota;
        v.heat = nota;
        v
    }

    #[test]
    fn respeta_el_hueco_minimo_entre_clips() {
        let clips = ClipsConfig { num_clips: 2, min_gap: 60.0, ..Default::default() };
        let mut ventanas = vec![ventana(0.0, "uno dos tres", 1.0), ventana(35.0, "cuatro cinco seis", 0.9), ventana(400.0, "siete ocho nueve", 0.8)];
        let elegidos = finalise(&mut ventanas, &ScoringConfig::default(), &clips, None);
        assert_eq!(elegidos.len(), 2);
        assert!(elegidos[1].start - elegidos[0].end > 0.0);
    }

    #[test]
    fn descarta_el_segundo_clip_del_mismo_tema() {
        let clips = ClipsConfig { num_clips: 3, min_gap: 10.0, ..Default::default() };
        let scoring = ScoringConfig { max_similarity: 0.5, ..Default::default() };
        let mismo = "bitcoin bitcoin bitcoin comprar comprar precio precio subida";
        let otro = "perros gatos veterinario vacunas paseo comida animales";
        let mut ventanas = vec![ventana(0.0, mismo, 1.0), ventana(200.0, mismo, 0.9), ventana(400.0, otro, 0.8)];
        let elegidos = finalise(&mut ventanas, &scoring, &clips, None);
        assert_eq!(elegidos.len(), 2);
        assert!(elegidos.last().unwrap().text.contains("perros"));
    }

    #[test]
    fn el_limite_manda_sobre_num_clips() {
        let clips = ClipsConfig { num_clips: 2, min_gap: 10.0, ..Default::default() };
        let mut ventanas: Vec<Window> = (0..6).map(|i| ventana(i as f64 * 100.0, &format!("tema{i} palabra{i} distinto{i}"), 1.0 - i as f64 / 10.0)).collect();
        assert_eq!(finalise(&mut ventanas, &ScoringConfig::default(), &clips, Some(5)).len(), 5);
    }

    #[test]
    fn los_elegidos_salen_en_orden_cronologico_con_su_nota() {
        let clips = ClipsConfig { num_clips: 3, min_gap: 10.0, ..Default::default() };
        let mut ventanas = vec![ventana(300.0, "a1 b1 c1", 0.5), ventana(0.0, "a2 b2 c2", 1.0), ventana(600.0, "a3 b3 c3", 0.2)];
        let elegidos = finalise(&mut ventanas, &ScoringConfig::default(), &clips, None);
        assert!(elegidos.windows(2).all(|w| w[0].start < w[1].start));
        assert!(elegidos.iter().all(|w| w.score >= 0.0));
        assert_eq!(ventanas.iter().map(|w| w.score).fold(0.0, f64::max), 1.0);
    }

    #[test]
    fn una_senal_constante_no_cuenta_como_cero() {
        // Sin heatmap (todo a 0) las notas de Claude deciden solas.
        let mut a = Window::new(0.0, 30.0, vec![], "a");
        a.llm_content = 0.9;
        let mut b = Window::new(100.0, 130.0, vec![], "b");
        b.llm_content = 0.1;
        let clips = ClipsConfig { num_clips: 1, ..Default::default() };
        let elegidos = finalise(&mut [a, b], &ScoringConfig::default(), &clips, None);
        assert_eq!(elegidos[0].start, 0.0);
    }

    #[test]
    fn la_lista_corta_para_claude_no_trae_clones_solapados() {
        let mut ventanas: Vec<Window> = (0..20).map(|i| {
            let mut v = Window::new(i as f64 * 2.0, i as f64 * 2.0 + 30.0, vec![], format!("v{i}"));
            v.heat = i as f64;
            v
        }).collect();
        ventanas.push(Window { heat: 5.0, ..Window::new(500.0, 530.0, vec![], "lejos") });
        let lista = shortlist_for_llm(&ventanas, 10, &ScoringConfig::default());
        for (i, a) in lista.iter().enumerate() {
            for b in &lista[i + 1..] {
                assert!(!a.overlaps(b, 0.0));
            }
        }
        assert!(lista.windows(2).all(|w| w[0].start <= w[1].start));
    }

    #[test]
    fn las_notas_de_claude_llegan_a_su_ventana() {
        let mut v = vec![Window::default(), Window::default()];
        let notas = HashMap::from([(1usize, LlmScore { content: 0.8, hook: 0.6, reason: "buen giro".into() }), (9, LlmScore { content: 1.0, hook: 1.0, reason: String::new() })]);
        apply_llm_scores(&mut v, &notas);
        assert_eq!((v[1].llm_content, v[1].llm_hook, v[1].reason.as_str()), (0.8, 0.6, "buen giro"));
        assert_eq!(v[0].llm_content, 0.0);
    }

    #[test]
    fn senales_baratas() {
        let mut v = vec![Window::new(0.0, 10.0, vec![Word::new("a", 0.0, 1.0); 20], "x")];
        apply_cheap_signals(&mut v, &[HeatPoint { start: 0.0, end: 10.0, value: 0.7 }], None);
        assert!((v[0].heat - 0.7).abs() < 1e-12);
        assert_eq!(v[0].energy, 0.0);
        assert_eq!(v[0].density, 2.0);
    }

    // -- utilidades ----------------------------------------------------------

    #[test]
    fn huellas_y_parecido() {
        let a = fingerprint("El Bitcoin sube, el bitcoin baja; ¡qué ÁNIMO tiene el precio!");
        assert_eq!(a.get("bitcoin"), Some(&2.0));
        assert!(a.contains_key("animo") && a.contains_key("precio"));
        assert!(!a.contains_key("que") && !a.contains_key("sube") == false);
        assert!((cosine(&a, &a) - 1.0).abs() < 1e-12);
        assert_eq!(cosine(&a, &fingerprint("perros gatos veterinario")), 0.0);
        assert_eq!(cosine(&a, &HashMap::new()), 0.0);
    }

    #[test]
    fn solape_y_formato() {
        let a = Window::new(0.0, 30.0, vec![], "");
        let b = Window::new(40.0, 70.0, vec![], "");
        assert!(!a.overlaps(&b, 0.0));
        assert!(a.overlaps(&b, 11.0));
        assert!(!a.overlaps(&b, 10.0), "el limite es estricto");
        assert_eq!(hms(75.0), "1:15");
        assert_eq!(hms(3725.0), "1:02:05");
        assert!(a.describe().contains("0:00-0:30 (30s)"));
    }
}
