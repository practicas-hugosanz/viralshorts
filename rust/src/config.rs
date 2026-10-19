//! Configuracion del pipeline: valores por defecto + carga desde YAML.
//!
//! Todos los parametros ajustables viven aqui. `Config::load` mezcla en
//! profundidad el YAML del usuario sobre los valores por defecto, de modo que
//! el fichero solo necesita contener lo que se quiera cambiar. Los nombres de
//! los campos son los de `config.yaml`, los mismos que en la version Python.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("No se encontro el fichero de configuracion: {}", .0.display())]
    NotFound(PathBuf),
    #[error("{0}")]
    Invalid(String),
}

// ----------------------------------------------------------------------------
// Grupos de configuracion
// ----------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DownloadConfig {
    /// Preferimos <=1080p: bajar 4K de un podcast de 3h no aporta nada al short.
    pub max_height: i64,
    pub format: String,
    /// Navegador del que leer cookies (chrome, firefox, edge...). None = sin cookies.
    pub cookies_from_browser: Option<String>,
    pub cookiefile: Option<String>,
    pub retries: i64,
    /// Fragmentos DASH en paralelo: es lo que acelera la bajada.
    pub concurrent_fragments: i64,
    /// Trozos de 10 MB: una conexion que se corta no obliga a repetir el fragmento.
    pub http_chunk_size: i64,
}

impl Default for DownloadConfig {
    fn default() -> Self {
        Self {
            max_height: 1080,
            format: "bestvideo[height<={max_height}][ext=mp4]+bestaudio[ext=m4a]/\
                     best[height<={max_height}]/best"
                .into(),
            cookies_from_browser: None,
            cookiefile: None,
            retries: 5,
            concurrent_fragments: 8,
            http_chunk_size: 10_485_760,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TranscribeConfig {
    /// tiny | base | small | medium | large-v3
    pub model_size: String,
    /// None = autodetectar
    pub language: Option<String>,
    /// auto | cpu | cuda
    pub device: String,
    /// Se conserva por compatibilidad con config.yaml; whisper.cpp elige solo.
    pub compute_type: String,
    /// 1 (greedy) transcribe ~2x mas rapido que 5 con diferencia despreciable.
    pub beam_size: i64,
    pub vad_filter: bool,
    /// Hilos de CPU. 0 = auto.
    pub cpu_threads: i64,
    pub condition_on_previous_text: bool,
    pub batched: bool,
    pub batch_size: i64,
    /// Modelo mejor con el que se vuelve a transcribir SOLO el trozo de cada clip
    /// elegido (unos segundos de audio, no horas). "" = no se afina.
    pub refine_model: String,
    /// Nombres propios y palabras raras del pódcast, para que whisper las escriba
    /// bien. Va como `--prompt`.
    pub initial_prompt: String,
    /// Correcciones fijas, `mal=bien;otra mal=otra bien`, que se aplican al texto.
    pub corrections: String,
}

impl Default for TranscribeConfig {
    fn default() -> Self {
        Self {
            model_size: "medium".into(),
            language: Some("es".into()),
            device: "auto".into(),
            compute_type: "auto".into(),
            beam_size: 1,
            vad_filter: true,
            cpu_threads: 0,
            condition_on_previous_text: false,
            batched: true,
            batch_size: 8,
            refine_model: String::new(),
            initial_prompt: String::new(),
            corrections: String::new(),
        }
    }
}

/// Preseleccion de zonas SIN transcribir: el heatmap de YouTube y la energia
/// del audio dicen donde estan los momentos calientes, y solo se transcriben
/// esos.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PrescanConfig {
    pub enabled: bool,
    /// Zonas candidatas a transcribir. 0 = auto (num_clips * 5, minimo 12).
    pub regions: i64,
    /// Margen extra a cada lado de la zona.
    pub padding: f64,
    /// Paso del barrido grueso, en segundos.
    pub step: f64,
}

impl Default for PrescanConfig {
    fn default() -> Self {
        Self { enabled: true, regions: 0, padding: 15.0, step: 5.0 }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClipsConfig {
    pub num_clips: i64,
    pub min_duration: f64,
    pub max_duration: f64,
    /// Segundos iniciales / finales que se ignoran (intros, despedidas).
    pub head_skip: f64,
    pub tail_skip: f64,
    /// Separacion minima entre dos clips seleccionados.
    pub min_gap: f64,
}

impl Default for ClipsConfig {
    fn default() -> Self {
        Self {
            num_clips: 3,
            min_duration: 25.0,
            max_duration: 55.0,
            head_skip: 60.0,
            tail_skip: 30.0,
            min_gap: 30.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ScoringConfig {
    /// Pesos de cada senal; se normalizan solos.
    pub weight_heatmap: f64,
    pub weight_energy: f64,
    pub weight_density: f64,
    pub weight_context: f64,
    pub weight_llm: f64,
    pub weight_hook: f64,
    /// Ventanas candidatas que se mandan al LLM.
    pub llm_candidates: i64,
    /// Fraccion de vocabulario compartido a partir de la cual el segundo clip
    /// se descarta. 1.0 desactiva la comprobacion.
    pub max_similarity: f64,
}

impl Default for ScoringConfig {
    fn default() -> Self {
        Self {
            weight_heatmap: 0.30,
            weight_energy: 0.15,
            weight_density: 0.05,
            weight_context: 0.25,
            weight_llm: 0.35,
            weight_hook: 0.15,
            llm_candidates: 24,
            max_similarity: 0.5,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LlmConfig {
    pub enabled: bool,
    pub model: String,
    /// low | medium | high | xhigh | max
    pub effort: String,
    pub max_tokens: i64,
    pub generate_metadata: bool,
    pub max_retries: i64,
    /// Segundos; se duplica en cada intento.
    pub retry_delay: f64,
    pub batch_size: i64,
    pub metadata_batch_size: i64,
    pub cache: bool,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            model: "claude-opus-5-5".into(),
            effort: "medium".into(),
            max_tokens: 16000,
            generate_metadata: true,
            max_retries: 3,
            retry_delay: 3.0,
            batch_size: 10,
            metadata_batch_size: 6,
            cache: true,
        }
    }
}

/// Deteccion de donde EMPIEZA un tema, para que el clip se entienda solo.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ContextConfig {
    pub enabled: bool,
    pub block_seconds: f64,
    pub compare_blocks: i64,
    pub sensitivity: f64,
    pub boundary_grace: f64,
    pub intro_seconds: f64,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            block_seconds: 12.0,
            compare_blocks: 5,
            sensitivity: 0.6,
            boundary_grace: 8.0,
            intro_seconds: 15.0,
        }
    }
}

/// Textos de publicacion cuando NO hay modelo de lenguaje (y para rellenar los
/// huecos que deje Claude).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MetadataConfig {
    pub enabled: bool,
    pub max_title: i64,
    pub max_hook: i64,
    pub max_description: i64,
    pub hashtags: i64,
    pub hook_window: f64,
    pub min_hook_score: f64,
    pub min_title_score: f64,
}

impl Default for MetadataConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_title: 70,
            max_hook: 45,
            max_description: 160,
            hashtags: 6,
            hook_window: 12.0,
            min_hook_score: 3.5,
            min_title_score: 2.4,
        }
    }
}

/// Reencuadre del panel superior siguiendo al que habla.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReframeConfig {
    pub enabled: bool,
    pub sample_interval: f64,
    pub smoothing_window: f64,
    pub deadzone: f64,
    pub max_keyframes: i64,
    pub ignore_static_faces: bool,
    pub min_face_motion: f64,
    pub static_ratio: f64,
    pub min_face_size: f64,
    pub speaker_tracking: bool,
    pub talk_window: f64,
    pub switch_margin: f64,
    pub switch_hold: i64,
    /// Confianza minima de YuNet.
    pub score_threshold: f64,
    /// Modelo YuNet. Se descarga solo la primera vez a paths.cache_dir.
    pub model_path: Option<String>,
    pub model_url: String,
}

impl Default for ReframeConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            sample_interval: 0.5,
            smoothing_window: 2.0,
            deadzone: 0.04,
            max_keyframes: 60,
            ignore_static_faces: true,
            min_face_motion: 1.6,
            static_ratio: 0.35,
            min_face_size: 0.06,
            speaker_tracking: true,
            talk_window: 2.5,
            switch_margin: 1.4,
            switch_hold: 2,
            score_threshold: 0.6,
            model_path: None,
            model_url: "https://github.com/opencv/opencv_zoo/raw/main/models/\
                        face_detection_yunet/face_detection_yunet_2023mar.onnx"
                .into(),
        }
    }
}

/// Punch-in suave para que el plano no se quede estatico.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ZoomConfig {
    pub enabled: bool,
    pub min_zoom: f64,
    pub max_zoom: f64,
    /// Segundos por ciclo completo de zoom.
    pub period: f64,
}

impl Default for ZoomConfig {
    fn default() -> Self {
        Self { enabled: true, min_zoom: 1.0, max_zoom: 1.07, period: 14.0 }
    }
}

/// Eliminacion de silencios para subir el ritmo.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct JumpCutsConfig {
    pub enabled: bool,
    /// Silencio maximo que se conserva.
    pub max_silence: f64,
    /// Margen alrededor de cada palabra.
    pub padding: f64,
    /// Descarta trozos conservados mas cortos que esto.
    pub min_keep: f64,
}

impl Default for JumpCutsConfig {
    fn default() -> Self {
        Self { enabled: true, max_silence: 0.35, padding: 0.12, min_keep: 0.20 }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LayoutConfig {
    pub width: i64,
    pub height: i64,
    /// Proporcion de altura para el panel del podcast.
    pub top_ratio: f64,
    pub fps: i64,
}

impl Default for LayoutConfig {
    fn default() -> Self {
        Self { width: 1080, height: 1920, top_ratio: 0.5, fps: 30 }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioConfig {
    pub loudnorm: bool,
    /// LUFS objetivo (estandar de redes sociales).
    pub target_i: f64,
    pub target_tp: f64,
    pub target_lra: f64,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self { loudnorm: true, target_i: -14.0, target_tp: -1.5, target_lra: 11.0 }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SubtitlesConfig {
    pub enabled: bool,
    pub font_name: String,
    pub font_size: i64,
    pub uppercase: bool,
    pub color_normal: String,
    pub color_active: String,
    pub color_outline: String,
    pub color_shadow: String,
    pub outline_width: i64,
    pub shadow_depth: i64,
    pub max_words_per_group: i64,
    /// 0 = abajo del todo, 1 = arriba.
    pub vertical_position_ratio: f64,
    pub remove_punctuation: bool,
    pub punctuation_chars: String,
    /// % de escala de la palabra activa (100 = sin pop).
    pub pop_scale: i64,
    pub pop_duration: f64,
    pub censor_profanity: bool,
    pub censor_char: String,
    pub censor_words: Vec<String>,
    /// Carpeta con .ttf propios (opcional).
    pub fonts_dir: Option<String>,
}

impl Default for SubtitlesConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            font_name: "Arial Black".into(),
            font_size: 92,
            uppercase: true,
            color_normal: "#FFFFFF".into(),
            color_active: "#FFF200".into(),
            color_outline: "#000000".into(),
            color_shadow: "#000000".into(),
            outline_width: 5,
            shadow_depth: 2,
            max_words_per_group: 3,
            vertical_position_ratio: 0.30,
            remove_punctuation: true,
            punctuation_chars: ".,!?¡¿…\";:".into(),
            pop_scale: 118,
            pop_duration: 0.12,
            censor_profanity: true,
            censor_char: "#".into(),
            censor_words: Vec::new(),
            fonts_dir: None,
        }
    }
}

/// Titular fijo en la parte superior durante los primeros segundos.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HookConfig {
    pub enabled: bool,
    pub duration: f64,
    pub font_size: i64,
    pub font_name: String,
    pub color: String,
    pub color_outline: String,
    pub top_margin_ratio: f64,
    pub uppercase: bool,
    pub max_chars: i64,
}

impl Default for HookConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            duration: 4.0,
            font_size: 64,
            font_name: "Arial Black".into(),
            color: "#FFFFFF".into(),
            color_outline: "#000000".into(),
            top_margin_ratio: 0.06,
            uppercase: true,
            max_chars: 70,
        }
    }
}

/// Tarjeta de contexto al principio del video, en la costura entre los dos
/// paneles. Nunca coincide en pantalla con el gancho.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TitleCardConfig {
    pub enabled: bool,
    /// De donde sale el texto: `video` (titulo entero), `show` (programa e
    /// invitado) o `clip` (la mejor frase del propio clip).
    pub source: String,
    pub duration: f64,
    pub fade: f64,
    /// Vacio = la misma que los subtitulos.
    pub font_name: String,
    pub font_size: i64,
    pub uppercase: bool,
    pub color: String,
    pub color_outline: String,
    pub outline_width: i64,
    pub shadow_depth: i64,
    pub max_chars: i64,
    pub wrap_chars: i64,
}

impl Default for TitleCardConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            source: "show".into(),
            duration: 2.5,
            fade: 0.25,
            font_name: String::new(),
            font_size: 74,
            uppercase: false,
            color: "#FFFFFF".into(),
            color_outline: "#000000".into(),
            outline_width: 6,
            shadow_depth: 2,
            max_chars: 90,
            wrap_chars: 22,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OutputConfig {
    pub dir: String,
    pub crf: i64,
    pub preset: String,
    pub video_codec: String,
    pub audio_bitrate: String,
    /// "auto" prueba NVENC, QSV y AMF (comprobando que funcionen de verdad) y
    /// cae a libx264; "off" fuerza libx264; tambien admite un nombre concreto.
    pub hardware_encoder: String,
    /// Compatibilidad: True equivale a hardware_encoder: h264_nvenc.
    pub use_nvenc: bool,
    /// Renders simultaneos. 0 = auto segun los nucleos.
    pub render_workers: i64,
    pub thumbnail: bool,
    pub thumbnail_at: f64,
    pub csv: bool,
    /// Una subcarpeta por ejecucion dentro de `dir`.
    pub run_subfolder: bool,
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            dir: "output".into(),
            crf: 20,
            preset: "medium".into(),
            video_codec: "libx264".into(),
            audio_bitrate: "192k".into(),
            hardware_encoder: "auto".into(),
            use_nvenc: false,
            render_workers: 0,
            thumbnail: true,
            thumbnail_at: 1.2,
            csv: true,
            run_subfolder: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PathsConfig {
    /// Descargas + cache de transcripciones.
    pub work_dir: String,
    pub cache_dir: String,
    /// Selecciones guardadas para revisar.
    pub plans_dir: String,
}

impl Default for PathsConfig {
    fn default() -> Self {
        Self {
            work_dir: "work".into(),
            cache_dir: "work/cache".into(),
            plans_dir: "work/plans".into(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub download: DownloadConfig,
    pub transcribe: TranscribeConfig,
    pub prescan: PrescanConfig,
    pub clips: ClipsConfig,
    pub scoring: ScoringConfig,
    pub context: ContextConfig,
    pub llm: LlmConfig,
    pub metadata: MetadataConfig,
    pub reframe: ReframeConfig,
    pub zoom: ZoomConfig,
    pub jumpcuts: JumpCutsConfig,
    pub layout: LayoutConfig,
    pub audio: AudioConfig,
    pub subtitles: SubtitlesConfig,
    pub hook: HookConfig,
    pub title_card: TitleCardConfig,
    pub output: OutputConfig,
    pub paths: PathsConfig,
}

impl Config {
    /// Valores por defecto, con el YAML encima si hay ruta.
    pub fn load(path: Option<&Path>) -> Result<Self, ConfigError> {
        let mut cfg = Self::default();
        let Some(path) = path else { return Ok(cfg) };

        if !path.is_file() {
            return Err(ConfigError::NotFound(path.to_path_buf()));
        }
        let origin = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        let text = fs::read_to_string(path)
            .map_err(|e| ConfigError::Invalid(format!("{origin}: no se pudo leer ({e})")))?;
        cfg.merge_yaml(&text, &origin)?;
        Ok(cfg)
    }

    pub fn merge_yaml(&mut self, text: &str, origin: &str) -> Result<(), ConfigError> {
        let user: Value = serde_yaml_ng::from_str(text)
            .map_err(|e| ConfigError::Invalid(format!("{origin}: YAML invalido ({e})")))?;
        let user = if user.is_null() { json!({}) } else { user };
        if !user.is_object() {
            return Err(ConfigError::Invalid(format!(
                "{origin} debe contener un mapa YAML en la raiz."
            )));
        }
        self.merge(&user, origin)
    }

    /// Vuelca `user` (un mapa) sobre la configuracion actual, validando que
    /// cada clave exista. Es lo que usa el YAML y tambien el plan guardado.
    pub fn merge(&mut self, user: &Value, origin: &str) -> Result<(), ConfigError> {
        let mut base = self.to_value();
        check_keys(&base, user, origin, "")?;
        deep_merge(&mut base, user);
        *self = serde_json::from_value(base)
            .map_err(|e| ConfigError::Invalid(format!("{origin}: ajuste invalido ({e})")))?;
        Ok(())
    }

    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).expect("la configuracion siempre es serializable")
    }

    /// `--set subtitles.font_size=80`, con el tipo del ajuste de destino.
    pub fn set(&mut self, item: &str) -> Result<(), ConfigError> {
        let bad = |msg: String| ConfigError::Invalid(msg);
        let Some((dotted, raw)) = item.split_once('=') else {
            return Err(bad(format!("--set espera clave=valor, no '{item}'.")));
        };
        let dotted = dotted.trim();
        let parts: Vec<&str> = dotted.split('.').map(str::trim).filter(|p| !p.is_empty()).collect();
        if parts.is_empty() {
            return Err(bad(format!("--set: clave vacia en '{item}'.")));
        }

        let mut tree = self.to_value();
        let mut node = &mut tree;
        for part in &parts {
            node = node
                .as_object_mut()
                .and_then(|map| map.get_mut(*part))
                .ok_or_else(|| bad(format!("--set: no existe el ajuste '{dotted}'.")))?;
        }
        if node.is_object() {
            return Err(bad(format!(
                "--set: '{dotted}' es un grupo de ajustes, no un valor."
            )));
        }
        *node = coerce(node, raw.trim())?;

        *self = serde_json::from_value(tree).map_err(|e| {
            bad(format!("--set: el valor de '{dotted}' no es valido ({e})."))
        })?;
        Ok(())
    }
}

/// Error si el usuario trae una clave que la configuracion no conoce.
fn check_keys(base: &Value, user: &Value, origin: &str, path: &str) -> Result<(), ConfigError> {
    let (Some(known), Some(given)) = (base.as_object(), user.as_object()) else {
        return Ok(());
    };
    for (key, value) in given {
        let Some(current) = known.get(key) else {
            let mut valid: Vec<&str> = known.keys().map(String::as_str).collect();
            valid.sort_unstable();
            let section = if path.is_empty() { "la raiz".to_string() } else { path.to_string() };
            return Err(ConfigError::Invalid(format!(
                "{origin}: clave desconocida '{key}' en {section}. Validas: {}",
                valid.join(", ")
            )));
        };
        let child = if path.is_empty() { key.clone() } else { format!("{path}.{key}") };
        check_keys(current, value, origin, &child)?;
    }
    Ok(())
}

fn deep_merge(base: &mut Value, user: &Value) {
    match (base, user) {
        (Value::Object(base_map), Value::Object(user_map)) => {
            for (key, value) in user_map {
                match base_map.get_mut(key) {
                    Some(slot) => deep_merge(slot, value),
                    None => {
                        base_map.insert(key.clone(), value.clone());
                    }
                }
            }
        }
        // `seccion:` vacia en el YAML: se queda con lo que habia.
        (Value::Object(_), Value::Null) => {}
        (slot, value) => *slot = value.clone(),
    }
}

/// Convierte el texto de la linea de comandos al tipo que ya tiene el campo.
fn coerce(current: &Value, raw: &str) -> Result<Value, ConfigError> {
    let bad = |msg: String| ConfigError::Invalid(msg);
    let lowered = raw.to_lowercase();
    if lowered == "null" || lowered == "none" {
        return Ok(Value::Null);
    }
    Ok(match current {
        Value::Bool(_) => match lowered.as_str() {
            "1" | "true" | "si" | "sí" | "yes" | "on" => Value::Bool(true),
            "0" | "false" | "no" | "off" => Value::Bool(false),
            _ => return Err(bad(format!("--set: '{raw}' no es si/no."))),
        },
        Value::Number(n) if n.is_f64() => {
            let value: f64 = raw
                .parse()
                .map_err(|_| bad(format!("--set: '{raw}' no es un numero.")))?;
            json!(value)
        }
        Value::Number(_) => {
            let value: f64 = raw
                .parse()
                .map_err(|_| bad(format!("--set: '{raw}' no es un numero entero.")))?;
            json!(value.trunc() as i64)
        }
        Value::Array(_) => Value::Array(
            raw.split(',')
                .map(str::trim)
                .filter(|part| !part.is_empty())
                .map(|part| Value::String(part.to_string()))
                .collect(),
        ),
        _ => Value::String(raw.to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn yaml(text: &str) -> Result<Config, ConfigError> {
        let mut cfg = Config::default();
        cfg.merge_yaml(text, "c.yaml")?;
        Ok(cfg)
    }

    #[test]
    fn el_yaml_solo_pisa_lo_que_trae() {
        let cfg = yaml("clips:\n  num_clips: 7\n").unwrap();
        assert_eq!(cfg.clips.num_clips, 7);
        assert_eq!(cfg.clips.min_duration, Config::default().clips.min_duration);
    }

    #[test]
    fn enteros_en_campos_decimales() {
        let cfg = yaml("clips:\n  min_duration: 20\n  head_skip: 0\n").unwrap();
        assert_eq!(cfg.clips.min_duration, 20.0);
        assert_eq!(cfg.clips.head_skip, 0.0);
    }

    #[test]
    fn una_clave_inventada_falla_con_un_mensaje_claro() {
        let error = yaml("clips:\n  numero_de_clips: 7\n").unwrap_err().to_string();
        assert!(error.contains("numero_de_clips"), "{error}");
        assert!(error.contains("clips"), "{error}");
        assert!(error.contains("num_clips"), "debe listar las validas: {error}");
    }

    #[test]
    fn una_seccion_inventada_tambien() {
        let error = yaml("subtitulos:\n  tamano: 3\n").unwrap_err().to_string();
        assert!(error.contains("subtitulos"), "{error}");
    }

    #[test]
    fn un_tipo_equivocado_falla() {
        assert!(yaml("clips:\n  num_clips: muchos\n").is_err());
    }

    #[test]
    fn yaml_vacio_o_con_seccion_vacia() {
        assert_eq!(yaml("").unwrap(), Config::default());
        assert_eq!(yaml("clips:\n").unwrap(), Config::default());
        assert!(yaml("- una\n- lista\n").is_err());
    }

    #[test]
    fn null_explicito_apaga_un_opcional() {
        let cfg = yaml("transcribe:\n  language: null\n").unwrap();
        assert_eq!(cfg.transcribe.language, None);
        assert_eq!(Config::default().transcribe.language.as_deref(), Some("es"));
    }

    #[test]
    fn set_convierte_al_tipo_del_ajuste() {
        let mut cfg = Config::default();
        cfg.set("subtitles.font_size=80").unwrap();
        cfg.set("subtitles.vertical_position_ratio=0.42").unwrap();
        cfg.set("llm.enabled=false").unwrap();
        cfg.set("subtitles.censor_words=chorra, tonto").unwrap();
        cfg.set("transcribe.language=null").unwrap();
        cfg.set("output.hardware_encoder=off").unwrap();

        assert_eq!(cfg.subtitles.font_size, 80);
        assert!((cfg.subtitles.vertical_position_ratio - 0.42).abs() < 1e-9);
        assert!(!cfg.llm.enabled);
        assert_eq!(cfg.subtitles.censor_words, vec!["chorra", "tonto"]);
        assert_eq!(cfg.transcribe.language, None);
        assert_eq!(cfg.output.hardware_encoder, "off");
    }

    #[test]
    fn set_acepta_decimales_en_enteros_como_hacia_python() {
        let mut cfg = Config::default();
        cfg.set("subtitles.font_size=80.0").unwrap();
        assert_eq!(cfg.subtitles.font_size, 80);
    }

    #[test]
    fn set_rechaza_ajustes_que_no_existen() {
        for malo in ["subtitulos.tamano=80", "subtitles.tamano=80", "sin_igual", "=3", "subtitles=3"] {
            let mut cfg = Config::default();
            assert!(cfg.set(malo).is_err(), "{malo}");
            assert_eq!(cfg, Config::default(), "{malo} no debe dejar nada a medias");
        }
    }

    #[test]
    fn set_rechaza_valores_del_tipo_equivocado() {
        let mut cfg = Config::default();
        assert!(cfg.set("llm.enabled=quizas").is_err());
        assert!(cfg.set("subtitles.font_size=grande").is_err());
        assert!(cfg.set("subtitles.font_size=null").is_err());
        assert_eq!(cfg, Config::default());
    }

    #[test]
    fn el_config_yaml_del_proyecto_se_entiende() {
        let ruta = Path::new(env!("CARGO_MANIFEST_DIR")).join("../config.yaml");
        let cfg = Config::load(Some(&ruta)).expect("config.yaml debe cargar");
        assert_eq!(cfg.clips.num_clips, 3);
        assert_eq!(cfg.clips.max_duration, 45.0);
        assert_eq!(cfg.llm.model, "claude-opus-5-5");
    }

    #[test]
    fn fichero_que_no_existe() {
        assert!(matches!(
            Config::load(Some(Path::new("no/existe.yaml"))),
            Err(ConfigError::NotFound(_))
        ));
    }

    #[test]
    fn ida_y_vuelta_por_json_no_pierde_nada() {
        let mut cfg = Config::default();
        cfg.set("clips.num_clips=9").unwrap();
        let value = cfg.to_value();
        let mut otra = Config::default();
        otra.merge(&value, "plan").unwrap();
        assert_eq!(cfg, otra);
    }
}
