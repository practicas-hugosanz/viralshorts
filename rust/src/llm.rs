//! Puntuacion de contenido y generacion de metadatos con Claude.
//!
//! La densidad de palabras mide quien habla mas rapido, no que momento engancha.
//! Aqui se le pasan a Claude las transcripciones de las ventanas candidatas y se
//! le pide que las puntue como lo haria un editor de shorts: gancho, tension,
//! remate, y si se entiende sin contexto previo.
//!
//! Todo el modulo degrada con elegancia: si no hay credenciales o falla la API,
//! el pipeline sigue con las senales heuristicas (heatmap + energia + densidad).
//!
//! Se habla con la API de mensajes por HTTP, sin SDK.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha1::{Digest, Sha1};

use crate::config::LlmConfig;
use crate::progress;
use crate::scoring::{LlmScore, Window};

/// Errores que merecen otro intento: la API saturada, la conexion caida, o el
/// limite de peticiones por minuto. Lo demas (credenciales, peticion invalida)
/// no mejora por reintentarlo.
const RETRYABLE: [&str; 13] = [
    "rate limit", "rate_limit", "overloaded", "timeout", "timed out", "connection", "502", "503", "504", "529",
    "internal server error", "apiconnection", "apitimeout",
];

const SCORING_SYSTEM: &str = "\
Eres editor jefe de un canal de shorts verticales (TikTok / Reels / YouTube \
Shorts) que recorta podcasts. Recibes fragmentos transcritos y los puntuas.

Para cada fragmento das dos notas de 0 a 10:

content_score - cuanto engancha el fragmento ENTERO:
  9-10  historia con giro, confesion, opinion muy polemica, dato que sorprende,
        discusion acalorada, consejo concreto y accionable.
  6-8   anecdota o idea con principio y final claros.
  3-5   conversacion normal, correcta pero olvidable.
  0-2   relleno, presentaciones, patrocinios, chistes internos, frases sueltas
        que no se entienden sin el contexto de antes.

hook_score - cuanto agarran los primeros 3 segundos (la primera frase):
  9-10  pregunta directa, afirmacion chocante, cifra concreta, \"nunca te han
        contado que...\".
  6-8   entra en materia sin rodeos.
  3-5   arranca a media frase pero se recupera.
  0-2   empieza con muletillas, \"eh\", \"bueno\", o respondiendo a algo que no se
        ha oido.

Penaliza sin piedad: nombres propios sin presentar, referencias a \"lo que \
dijimos antes\", y fragmentos cuya frase inicial esta cortada.

Se exigente: la mayoria de fragmentos de un podcast son un 4. Reserva el 9 y el \
10 para lo que de verdad pararia el scroll.";

const METADATA_SYSTEM: &str = "\
Escribes los textos de publicacion de shorts verticales recortados de podcasts.
Para cada clip devuelves:

- title: titulo del video, maximo 70 caracteres, sin comillas ni emojis, en el
  idioma del clip. Concreto y con la tension del clip, nunca generico.
- hook: rotulo que aparece SOBRE el video los primeros segundos. Maximo 45
  caracteres, en MAYUSCULAS, formulado como pregunta o afirmacion chocante.
  Es lo que decide si alguien se queda: tiene que dar curiosidad sin resolverla.
- description: 1 o 2 frases para el pie de publicacion.
- hashtags: entre 4 y 7, sin la almohadilla, en minusculas, sin espacios,
  mezclando 2 muy genericos del nicho con otros mas especificos del tema.

Nunca inventes datos que no esten en el clip. No uses clickbait que el clip no
cumpla: si el clip no responde a la pregunta del gancho, cambia el gancho.";

fn score_schema() -> Value {
    json!({
        "type": "object",
        "properties": {"clips": {"type": "array", "items": {
            "type": "object",
            "properties": {
                "id": {"type": "integer"},
                "content_score": {"type": "number"},
                "hook_score": {"type": "number"},
                "reason": {"type": "string"},
            },
            "required": ["id", "content_score", "hook_score", "reason"],
            "additionalProperties": false,
        }}},
        "required": ["clips"],
        "additionalProperties": false,
    })
}

fn metadata_schema() -> Value {
    json!({
        "type": "object",
        "properties": {"clips": {"type": "array", "items": {
            "type": "object",
            "properties": {
                "id": {"type": "integer"},
                "title": {"type": "string"},
                "hook": {"type": "string"},
                "description": {"type": "string"},
                "hashtags": {"type": "array", "items": {"type": "string"}},
            },
            "required": ["id", "title", "hook", "description", "hashtags"],
            "additionalProperties": false,
        }}},
        "required": ["clips"],
        "additionalProperties": false,
    })
}

/// Titulo, gancho, descripcion y hashtags de un clip.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ClipMetadata {
    pub title: String,
    pub hook: String,
    pub description: String,
    pub hashtags: Vec<String>,
}

impl ClipMetadata {
    pub fn new(title: &str, hook: &str, description: &str, hashtags: &[&str]) -> Self {
        Self {
            title: title.into(),
            hook: hook.into(),
            description: description.into(),
            hashtags: hashtags.iter().map(|t| t.to_string()).collect(),
        }
    }

    /// Pie de publicacion: la descripcion y, debajo, los hashtags.
    pub fn caption(&self) -> String {
        let tags: Vec<String> = self.hashtags.iter().map(|t| format!("#{}", t.trim_start_matches('#'))).collect();
        format!("{}\n\n{}", self.description, tags.join(" ")).trim().to_string()
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct LlmError(pub String);

/// Una peticion al modelo que devuelve el JSON pedido. Es un trait para probar
/// los lotes, la cache y los reintentos sin red.
pub trait Completion {
    fn complete(&self, system: &str, user: &str, schema: &Value) -> Result<Value, LlmError>;
}

// ----------------------------------------------------------------------------
// Cliente HTTP
// ----------------------------------------------------------------------------

pub struct AnthropicClient {
    base_url: String,
    auth: (String, String),
    model: String,
    max_tokens: i64,
    effort: String,
    agent: ureq::Agent,
}

impl AnthropicClient {
    /// Credenciales de ANTHROPIC_API_KEY o de ANTHROPIC_AUTH_TOKEN.
    pub fn from_env(cfg: &LlmConfig) -> Result<Self, String> {
        let base = std::env::var("ANTHROPIC_BASE_URL").ok().filter(|b| !b.is_empty()).unwrap_or_else(|| "https://api.anthropic.com".into());
        let key = std::env::var("ANTHROPIC_API_KEY").ok().filter(|k| !k.is_empty());
        let token = std::env::var("ANTHROPIC_AUTH_TOKEN").ok().filter(|t| !t.is_empty());
        let auth = match (key, token) {
            (Some(key), _) => ("x-api-key".to_string(), key),
            (None, Some(token)) => ("authorization".to_string(), format!("Bearer {token}")),
            (None, None) => return Err("no hay credenciales configuradas: falta ANTHROPIC_API_KEY".into()),
        };
        Ok(Self::new(&base, auth, cfg))
    }

    pub fn new(base_url: &str, auth: (String, String), cfg: &LlmConfig) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            auth,
            model: cfg.model.clone(),
            max_tokens: cfg.max_tokens,
            effort: cfg.effort.clone(),
            agent: ureq::AgentBuilder::new()
                .timeout_connect(Duration::from_secs(20))
                .timeout_read(Duration::from_secs(600))
                .user_agent("viralshorts")
                .build(),
        }
    }

    pub fn request_body(&self, system: &str, user: &str, schema: &Value) -> Value {
        json!({
            "model": self.model,
            "max_tokens": self.max_tokens,
            "system": system,
            "thinking": {"type": "adaptive"},
            "output_config": {"effort": self.effort, "format": {"type": "json_schema", "schema": schema}},
            "messages": [{"role": "user", "content": user}],
        })
    }
}

impl Completion for AnthropicClient {
    fn complete(&self, system: &str, user: &str, schema: &Value) -> Result<Value, LlmError> {
        let response = self
            .agent
            .post(&format!("{}/v1/messages", self.base_url))
            .set(&self.auth.0, &self.auth.1)
            .set("anthropic-version", "2023-06-01")
            .set("content-type", "application/json")
            .send_json(self.request_body(system, user, schema));

        let body: Value = match response {
            Ok(ok) => ok.into_json().map_err(|e| LlmError(format!("Respuesta ilegible de la API: {e}")))?,
            Err(ureq::Error::Status(code, response)) => {
                let text = response.into_string().unwrap_or_default();
                let detail = serde_json::from_str::<Value>(&text)
                    .ok()
                    .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
                    .unwrap_or(text);
                return Err(LlmError(format!("Error code: {code} - {detail}")));
            }
            Err(ureq::Error::Transport(transport)) => return Err(LlmError(format!("Connection error: {transport}"))),
        };

        if body["stop_reason"] == "refusal" {
            let detail = body["stop_details"]["explanation"].as_str().unwrap_or("");
            return Err(LlmError(format!("Claude declino la peticion. {detail}").trim().to_string()));
        }
        let text = body["content"]
            .as_array()
            .and_then(|blocks| blocks.iter().find(|b| b["type"] == "text"))
            .and_then(|b| b["text"].as_str())
            .filter(|t| !t.is_empty())
            .ok_or_else(|| LlmError("Claude no devolvio contenido de texto.".into()))?;

        log::debug!(
            "Claude: {} tokens entrada / {} salida",
            body["usage"]["input_tokens"],
            body["usage"]["output_tokens"]
        );
        serde_json::from_str(text).map_err(|e| LlmError(format!("Claude no devolvio JSON valido: {e}")))
    }
}

// ----------------------------------------------------------------------------
// Juez
// ----------------------------------------------------------------------------

type Factory = Box<dyn Fn(&LlmConfig) -> Result<Box<dyn Completion>, String>>;

/// Cliente de Claude para puntuar clips y escribir sus metadatos.
pub struct ClipJudge {
    cfg: LlmConfig,
    client: Option<Box<dyn Completion>>,
    factory: Factory,
    disabled_reason: Option<String>,
    cache: Cache,
    sleeper: Box<dyn Fn(Duration)>,
}

impl ClipJudge {
    pub fn new(cfg: LlmConfig, cache_dir: Option<&Path>) -> Self {
        let factory: Factory = Box::new(|cfg| AnthropicClient::from_env(cfg).map(|c| Box::new(c) as Box<dyn Completion>));
        Self::build(cfg, cache_dir, None, factory)
    }

    /// Con un cliente ya hecho (pruebas, o credenciales que vienen de otro lado).
    pub fn with_client(cfg: LlmConfig, cache_dir: Option<&Path>, client: Box<dyn Completion>) -> Self {
        Self::build(cfg, cache_dir, Some(client), Box::new(|_| Err("sin cliente".into())))
    }

    fn build(cfg: LlmConfig, cache_dir: Option<&Path>, client: Option<Box<dyn Completion>>, factory: Factory) -> Self {
        let cache = Cache::new(cache_dir, cfg.cache);
        Self { cfg, client, factory, disabled_reason: None, cache, sleeper: Box::new(std::thread::sleep) }
    }

    pub fn set_sleeper(&mut self, sleeper: Box<dyn Fn(Duration)>) {
        self.sleeper = sleeper;
    }

    pub fn enabled(&self) -> bool {
        self.cfg.enabled
    }

    /// Hay forma de llamar a la API (y no ha fallado ya en esta ejecucion).
    pub fn available(&mut self) -> bool {
        if self.disabled_reason.is_some() {
            return false;
        }
        if self.client.is_none() {
            match (self.factory)(&self.cfg) {
                Ok(client) => self.client = Some(client),
                Err(reason) => {
                    log::warn!("Claude no disponible ({reason}). Se puntuara solo con heatmap + energia + densidad.");
                    self.disabled_reason = Some(reason);
                    return false;
                }
            }
        }
        true
    }

    /// Una peticion, reintentando los fallos que si se arreglan solos.
    fn call(&self, system: &str, user: &str, schema: &Value) -> Result<Value, LlmError> {
        let attempts = self.cfg.max_retries.max(1);
        let delay = self.cfg.retry_delay.max(0.5);
        let client = self.client.as_ref().expect("available() se comprueba antes");

        let mut attempt = 1;
        loop {
            match client.complete(system, user, schema) {
                Ok(value) => return Ok(value),
                Err(error) if attempt >= attempts || !is_retryable(&error.0) => return Err(error),
                Err(error) => {
                    let wait = delay * 2f64.powi(attempt as i32 - 1);
                    log::warn!("Claude fallo ({}). Reintento {} de {attempts} en {wait:.0}s.", short(&error.0), attempt + 1);
                    (self.sleeper)(Duration::from_secs_f64(wait));
                    attempt += 1;
                }
            }
        }
    }

    /// Puntua las ventanas candidatas. Devuelve {indice -> nota}.
    ///
    /// Se manda por lotes, no todo de golpe: con dos docenas de fragmentos
    /// largos en una sola peticion la respuesta se corta por max_tokens y los
    /// ultimos se quedan sin nota. Lo ya puntuado en una ejecucion anterior sale
    /// de la cache y no se paga dos veces.
    pub fn score_windows(&mut self, windows: &[Window]) -> HashMap<usize, LlmScore> {
        if windows.is_empty() || !self.available() {
            return HashMap::new();
        }

        let mut scores: HashMap<usize, LlmScore> = HashMap::new();
        let mut pending: Vec<usize> = Vec::new();
        for (index, window) in windows.iter().enumerate() {
            match self.cache.get("score", &self.key(&window.text, "")) {
                Some(cached) => {
                    scores.insert(
                        index,
                        LlmScore {
                            content: cached["content"].as_f64().unwrap_or(0.0),
                            hook: cached["hook"].as_f64().unwrap_or(0.0),
                            reason: cached["reason"].as_str().unwrap_or("").to_string(),
                        },
                    );
                }
                None => pending.push(index),
            }
        }

        if !scores.is_empty() {
            log::info!("{} fragmentos ya puntuados antes; se reutilizan.", scores.len());
        }
        if pending.is_empty() {
            return scores;
        }

        let batch_size = self.cfg.batch_size.max(1) as usize;
        let batches: Vec<&[usize]> = pending.chunks(batch_size).collect();
        log::info!("Pidiendo a {} que puntue {} fragmentos ({} peticion(es))...", self.cfg.model, pending.len(), batches.len());

        for (number, batch) in batches.iter().enumerate() {
            progress::progress("claude", number as f64 / batches.len() as f64, &format!("Claude puntuando ({}/{})", number + 1, batches.len()), false);
            let fragments: Vec<String> = batch
                .iter()
                .map(|&i| format!("<fragmento id=\"{i}\" duracion=\"{:.0}s\">\n{}\n</fragmento>", windows[i].duration(), windows[i].text))
                .collect();
            let user = format!(
                "Puntua estos {} fragmentos de un podcast. Devuelve una entrada por fragmento, usando el mismo id.\n\n{}",
                batch.len(),
                fragments.join("\n\n")
            );

            let data = match self.call(SCORING_SYSTEM, &user, &score_schema()) {
                Ok(data) => data,
                Err(error) => {
                    log::warn!("Fallo la puntuacion con Claude ({error}). Se sigue con el heatmap y la energia del audio.");
                    self.disabled_reason = Some(error.0.clone());
                    progress::result(
                        "aviso",
                        json!({
                            "titulo": "Claude no ha podido puntuar los fragmentos",
                            "texto": format!(
                                "{}. Los momentos se eligen igualmente con el heatmap de YouTube, la energia del audio y el contexto, pero la seleccion es menos fina.",
                                short(&error.0)
                            ),
                        }),
                    );
                    break;
                }
            };

            let mut got = 0;
            for item in data["clips"].as_array().map(Vec::as_slice).unwrap_or(&[]) {
                let Some(index) = item["id"].as_i64().and_then(|i| usize::try_from(i).ok()) else { continue };
                if !batch.contains(&index) {
                    continue;
                }
                let score = LlmScore {
                    content: (item["content_score"].as_f64().unwrap_or(0.0) / 10.0).clamp(0.0, 1.0),
                    hook: (item["hook_score"].as_f64().unwrap_or(0.0) / 10.0).clamp(0.0, 1.0),
                    reason: item["reason"].as_str().unwrap_or("").trim().to_string(),
                };
                self.cache.put(
                    "score",
                    &self.key(&windows[index].text, ""),
                    json!({"content": score.content, "hook": score.hook, "reason": score.reason}),
                );
                scores.insert(index, score);
                got += 1;
            }
            if got < batch.len() {
                log::warn!("Claude no puntuo {} de {} fragmentos de este lote.", batch.len() - got, batch.len());
            }
        }

        self.cache.flush();
        progress::progress("claude", 1.0, "Claude ha terminado", true);
        scores
    }

    /// Genera titulo, gancho, descripcion y hashtags para cada clip.
    pub fn write_metadata(&mut self, clips: &[Window], video_title: &str) -> HashMap<usize, ClipMetadata> {
        if clips.is_empty() || !self.cfg.generate_metadata || !self.available() {
            return HashMap::new();
        }

        let mut out: HashMap<usize, ClipMetadata> = HashMap::new();
        let mut pending: Vec<usize> = Vec::new();
        for (index, clip) in clips.iter().enumerate() {
            match self.cache.get("meta", &self.key(&clip.text, video_title)).and_then(|v| serde_json::from_value::<ClipMetadata>(v).ok()) {
                Some(cached) => {
                    out.insert(index, cached);
                }
                None => pending.push(index),
            }
        }
        if pending.is_empty() {
            return out;
        }

        let batch_size = self.cfg.metadata_batch_size.max(1) as usize;
        let batches: Vec<&[usize]> = pending.chunks(batch_size).collect();
        log::info!("Generando titulos, ganchos y hashtags para {} clips ({} peticion(es))...", pending.len(), batches.len());

        for batch in batches {
            let items: Vec<String> = batch.iter().map(|&i| format!("<clip id=\"{i}\">\n{}\n</clip>", clips[i].text)).collect();
            let user = format!(
                "Podcast de origen: \"{video_title}\".\n\nEscribe los textos de publicacion de estos {} clips.\n\n{}",
                batch.len(),
                items.join("\n\n")
            );

            let data = match self.call(METADATA_SYSTEM, &user, &metadata_schema()) {
                Ok(data) => data,
                Err(error) => {
                    log::warn!("Fallo la generacion de metadatos ({error}). Se escriben por extraccion.");
                    progress::result(
                        "aviso",
                        json!({
                            "titulo": "Claude no ha podido escribir los textos",
                            "texto": format!("{}. Los titulos, ganchos y hashtags se sacan de las propias frases del clip.", short(&error.0)),
                        }),
                    );
                    break;
                }
            };

            for item in data["clips"].as_array().map(Vec::as_slice).unwrap_or(&[]) {
                let Some(index) = item["id"].as_i64().and_then(|i| usize::try_from(i).ok()) else { continue };
                if !batch.contains(&index) {
                    continue;
                }
                let meta = ClipMetadata {
                    title: item["title"].as_str().unwrap_or("").trim().to_string(),
                    hook: item["hook"].as_str().unwrap_or("").trim().to_string(),
                    description: item["description"].as_str().unwrap_or("").trim().to_string(),
                    hashtags: item["hashtags"]
                        .as_array()
                        .map(|tags| {
                            tags.iter()
                                .filter_map(Value::as_str)
                                .map(|t| t.trim_start_matches('#').trim().to_string())
                                .filter(|t| !t.is_empty())
                                .collect()
                        })
                        .unwrap_or_default(),
                };
                self.cache.put("meta", &self.key(&clips[index].text, video_title), serde_json::to_value(&meta).unwrap_or(Value::Null));
                out.insert(index, meta);
            }
        }

        self.cache.flush();
        out
    }

    /// Identifica una peticion por su contenido Y por como se pidio. Cambiar de
    /// modelo o de esfuerzo invalida lo guardado: es otra respuesta.
    fn key(&self, text: &str, extra: &str) -> String {
        let payload = [self.cfg.model.as_str(), self.cfg.effort.as_str(), extra, text].join("|");
        format!("{:x}", Sha1::digest(payload.as_bytes()))[..20].to_string()
    }
}

fn is_retryable(error: &str) -> bool {
    let lowered = error.to_lowercase();
    RETRYABLE.iter().any(|needle| lowered.contains(needle))
}

fn short(error: &str) -> String {
    let first = error.trim().lines().next().unwrap_or("");
    if first.is_empty() { "error".into() } else { first.chars().take(160).collect() }
}

// ----------------------------------------------------------------------------
// Cache
// ----------------------------------------------------------------------------

/// Respuestas ya pagadas, guardadas en disco entre ejecuciones. Un fichero unico
/// con todo dentro: son unos kilobytes por video y evita llenar la carpeta de
/// miles de ficheros diminutos.
struct Cache {
    enabled: bool,
    path: Option<PathBuf>,
    data: serde_json::Map<String, Value>,
    dirty: bool,
}

impl Cache {
    fn new(dir: Option<&Path>, enabled: bool) -> Self {
        let path = dir.map(|d| d.join("llm.json"));
        let mut data = serde_json::Map::new();
        if let (true, Some(path)) = (enabled, path.as_ref().filter(|p| p.is_file())) {
            match std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()) {
                Some(Value::Object(map)) => data = map,
                _ => log::debug!("Cache del modelo ilegible; se empieza de cero."),
            }
        }
        Self { enabled: enabled && path.is_some(), path, data, dirty: false }
    }

    fn get(&self, kind: &str, key: &str) -> Option<Value> {
        if !self.enabled {
            return None;
        }
        self.data.get(kind)?.get(key).cloned()
    }

    fn put(&mut self, kind: &str, key: &str, value: Value) {
        if !self.enabled {
            return;
        }
        let section = self.data.entry(kind.to_string()).or_insert_with(|| json!({}));
        if let Value::Object(map) = section {
            map.insert(key.to_string(), value);
            self.dirty = true;
        }
    }

    fn flush(&mut self) {
        let (true, true, Some(path)) = (self.enabled, self.dirty, self.path.as_ref()) else { return };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match std::fs::write(path, Value::Object(self.data.clone()).to_string()) {
            Ok(()) => self.dirty = false,
            Err(error) => log::debug!("No se pudo guardar la cache del modelo: {error}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::rc::Rc;
    use std::sync::{Arc, Mutex};

    // -- cliente de mentira ------------------------------------------------------

    /// Lee los id="N" de la peticion y contesta con notas para todos.
    struct Falso {
        peticiones: Rc<RefCell<Vec<Vec<usize>>>>,
        errores: RefCell<Vec<&'static str>>,
    }

    impl Falso {
        fn nuevo() -> (Self, Rc<RefCell<Vec<Vec<usize>>>>) {
            let peticiones = Rc::new(RefCell::new(Vec::new()));
            (Self { peticiones: peticiones.clone(), errores: RefCell::new(Vec::new()) }, peticiones)
        }
    }

    fn ids(user: &str) -> Vec<usize> {
        let mut ids = Vec::new();
        let mut rest = user;
        while let Some(pos) = rest.find("id=\"") {
            rest = &rest[pos + 4..];
            let end = rest.find('"').unwrap();
            ids.push(rest[..end].parse().unwrap());
            rest = &rest[end..];
        }
        ids
    }

    impl Completion for Falso {
        fn complete(&self, system: &str, user: &str, _schema: &Value) -> Result<Value, LlmError> {
            if let Some(error) = self.errores.borrow_mut().pop() {
                return Err(LlmError(error.into()));
            }
            let ids = ids(user);
            self.peticiones.borrow_mut().push(ids.clone());
            if system == SCORING_SYSTEM {
                Ok(json!({"clips": ids.iter().map(|i| json!({"id": i, "content_score": 8, "hook_score": 7, "reason": "vale"})).collect::<Vec<_>>()}))
            } else {
                Ok(json!({"clips": ids.iter().map(|i| json!({"id": i, "title": format!(" Titulo {i} "), "hook": "GANCHO", "description": "Desc.", "hashtags": ["#uno", " dos ", ""]})).collect::<Vec<_>>()}))
            }
        }
    }

    fn ventanas(cuantas: usize) -> Vec<Window> {
        (0..cuantas).map(|i| Window::new(i as f64 * 100.0, i as f64 * 100.0 + 30.0, vec![], format!("fragmento numero {i}"))).collect()
    }

    fn juez(falso: Falso, cfg: LlmConfig, cache: Option<&Path>) -> ClipJudge {
        let mut juez = ClipJudge::with_client(cfg, cache, Box::new(falso));
        juez.set_sleeper(Box::new(|_| {}));
        juez
    }

    // -- lotes, cache y fallos -------------------------------------------------------

    #[test]
    fn trocea_en_lotes() {
        let (falso, peticiones) = Falso::nuevo();
        let cfg = LlmConfig { batch_size: 4, cache: false, ..Default::default() };
        let notas = juez(falso, cfg, None).score_windows(&ventanas(10));

        assert_eq!(notas.len(), 10);
        assert_eq!(peticiones.borrow().iter().map(Vec::len).collect::<Vec<_>>(), [4, 4, 2]);
        assert!((notas[&0].content - 0.8).abs() < 1e-12 && (notas[&0].hook - 0.7).abs() < 1e-12);
        assert_eq!(notas[&3].reason, "vale");
    }

    #[test]
    fn la_segunda_vez_no_vuelve_a_preguntar() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = LlmConfig { batch_size: 4, ..Default::default() };

        let (primero, p1) = Falso::nuevo();
        juez(primero, cfg.clone(), Some(dir.path())).score_windows(&ventanas(6));
        assert_eq!(p1.borrow().iter().map(Vec::len).sum::<usize>(), 6);

        let (segundo, p2) = Falso::nuevo();
        let notas = juez(segundo, cfg, Some(dir.path())).score_windows(&ventanas(6));
        assert!(p2.borrow().is_empty());
        assert_eq!(notas.len(), 6);
    }

    #[test]
    fn cambiar_de_modelo_invalida_lo_guardado() {
        let dir = tempfile::tempdir().unwrap();
        let (primero, _) = Falso::nuevo();
        juez(primero, LlmConfig::default(), Some(dir.path())).score_windows(&ventanas(3));

        let (segundo, p2) = Falso::nuevo();
        let otro = LlmConfig { model: "claude-sonnet-5-5".into(), ..Default::default() };
        juez(segundo, otro, Some(dir.path())).score_windows(&ventanas(3));
        assert_eq!(p2.borrow().iter().map(Vec::len).sum::<usize>(), 3);
    }

    #[test]
    fn un_fallo_no_tira_la_ejecucion() {
        let (falso, _) = Falso::nuevo();
        falso.errores.borrow_mut().push("Error code: 529 overloaded");
        let cfg = LlmConfig { cache: false, max_retries: 1, ..Default::default() };
        let mut j = juez(falso, cfg, None);
        assert!(j.score_windows(&ventanas(3)).is_empty());
        assert!(!j.available(), "tras un fallo no se vuelve a intentar en esta ejecucion");
    }

    #[test]
    fn un_error_pasajero_se_reintenta_con_espera_creciente() {
        let (falso, peticiones) = Falso::nuevo();
        falso.errores.borrow_mut().extend(["Error code: 529 overloaded", "Connection error: timed out"]);
        let esperas = Rc::new(RefCell::new(Vec::new()));
        let cfg = LlmConfig { cache: false, max_retries: 3, retry_delay: 3.0, ..Default::default() };
        let mut j = juez(falso, cfg, None);
        let anotar = esperas.clone();
        j.set_sleeper(Box::new(move |d| anotar.borrow_mut().push(d.as_secs_f64())));

        assert_eq!(j.score_windows(&ventanas(2)).len(), 2);
        assert_eq!(*esperas.borrow(), [3.0, 6.0]);
        assert_eq!(peticiones.borrow().len(), 1);
    }

    #[test]
    fn un_error_de_credenciales_no_se_reintenta() {
        let (falso, _) = Falso::nuevo();
        falso.errores.borrow_mut().push("Error code: 401 - invalid x-api-key");
        let esperas = Rc::new(RefCell::new(0));
        let cfg = LlmConfig { cache: false, max_retries: 5, ..Default::default() };
        let mut j = juez(falso, cfg, None);
        let anotar = esperas.clone();
        j.set_sleeper(Box::new(move |_| *anotar.borrow_mut() += 1));
        assert!(j.score_windows(&ventanas(1)).is_empty());
        assert_eq!(*esperas.borrow(), 0);
    }

    #[test]
    fn sin_credenciales_no_se_puntua_y_no_se_rompe() {
        let mut j = ClipJudge::build(LlmConfig::default(), None, None, Box::new(|_| Err("no hay credenciales".into())));
        assert!(j.score_windows(&ventanas(3)).is_empty());
        assert!(j.write_metadata(&ventanas(3), "x").is_empty());
        assert!(!j.available());
    }

    #[test]
    fn solo_se_aceptan_los_id_del_lote_y_las_notas_se_acotan() {
        struct Desbocado;
        impl Completion for Desbocado {
            fn complete(&self, _: &str, _: &str, _: &Value) -> Result<Value, LlmError> {
                Ok(json!({"clips": [
                    {"id": 0, "content_score": 99, "hook_score": -5, "reason": " bien "},
                    {"id": 77, "content_score": 5, "hook_score": 5, "reason": "ajeno"},
                    {"id": "x", "content_score": 5},
                ]}))
            }
        }
        let cfg = LlmConfig { cache: false, ..Default::default() };
        let notas = ClipJudge::with_client(cfg, None, Box::new(Desbocado)).score_windows(&ventanas(2));
        assert_eq!(notas.len(), 1);
        assert_eq!((notas[&0].content, notas[&0].hook, notas[&0].reason.as_str()), (1.0, 0.0, "bien"));
    }

    // -- metadatos -------------------------------------------------------------------

    #[test]
    fn los_metadatos_salen_limpios_por_lotes_y_con_cache() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = LlmConfig { metadata_batch_size: 2, ..Default::default() };
        let (falso, peticiones) = Falso::nuevo();
        let mut j = juez(falso, cfg.clone(), Some(dir.path()));
        let meta = j.write_metadata(&ventanas(5), "El Programa");

        assert_eq!(peticiones.borrow().iter().map(Vec::len).collect::<Vec<_>>(), [2, 2, 1]);
        assert_eq!(meta[&3].title, "Titulo 3");
        assert_eq!(meta[&3].hashtags, ["uno", "dos"]);

        let (otro, p2) = Falso::nuevo();
        let meta2 = juez(otro, cfg, Some(dir.path())).write_metadata(&ventanas(5), "El Programa");
        assert!(p2.borrow().is_empty());
        assert_eq!(meta2, meta);

        // Otro programa de origen es otra peticion.
        let (otro, p3) = Falso::nuevo();
        juez(otro, LlmConfig::default(), Some(dir.path())).write_metadata(&ventanas(1), "Otro Programa");
        assert_eq!(p3.borrow().len(), 1);
    }

    #[test]
    fn sin_generate_metadata_no_se_pide_nada() {
        let (falso, peticiones) = Falso::nuevo();
        let cfg = LlmConfig { generate_metadata: false, ..Default::default() };
        assert!(juez(falso, cfg, None).write_metadata(&ventanas(2), "x").is_empty());
        assert!(peticiones.borrow().is_empty());
    }

    #[test]
    fn el_pie_lleva_descripcion_y_hashtags() {
        let m = ClipMetadata::new("T", "H", "Una descripcion.", &["podcast", "#clip"]);
        assert_eq!(m.caption(), "Una descripcion.\n\n#podcast #clip");
        assert_eq!(ClipMetadata::default().caption(), "");
    }

    #[test]
    fn cache_ilegible_se_ignora() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("llm.json"), "{roto").unwrap();
        let (falso, peticiones) = Falso::nuevo();
        juez(falso, LlmConfig::default(), Some(dir.path())).score_windows(&ventanas(2));
        assert_eq!(peticiones.borrow().len(), 1);
    }

    // -- HTTP de verdad contra un servidor local ---------------------------------------

    /// Servidor HTTP minimo: responde a cada peticion con la siguiente respuesta
    /// de la lista y guarda lo que recibio.
    fn servidor(respuestas: Vec<(u16, String)>) -> (String, Arc<Mutex<Vec<(String, String)>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let recibidas = Arc::new(Mutex::new(Vec::new()));
        let guardar = recibidas.clone();
        std::thread::spawn(move || {
            for (codigo, cuerpo) in respuestas {
                let (mut socket, _) = listener.accept().unwrap();
                let mut datos = Vec::new();
                let mut buffer = [0u8; 4096];
                let (cabecera, longitud) = loop {
                    let leidos = socket.read(&mut buffer).unwrap();
                    datos.extend_from_slice(&buffer[..leidos]);
                    if let Some(pos) = datos.windows(4).position(|w| w == b"\r\n\r\n") {
                        let cabecera = String::from_utf8_lossy(&datos[..pos]).to_string();
                        let longitud = cabecera.lines().find_map(|l| l.to_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap())).unwrap_or(0);
                        break (cabecera, longitud + pos + 4);
                    }
                };
                while datos.len() < longitud {
                    let leidos = socket.read(&mut buffer).unwrap();
                    datos.extend_from_slice(&buffer[..leidos]);
                }
                let inicio = datos.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
                let cuerpo_recibido = String::from_utf8_lossy(&datos[inicio..]).to_string();
                guardar.lock().unwrap().push((cabecera, cuerpo_recibido));
                let respuesta = format!(
                    "HTTP/1.1 {codigo} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{cuerpo}",
                    cuerpo.len()
                );
                socket.write_all(respuesta.as_bytes()).unwrap();
            }
        });
        (url, recibidas)
    }

    fn cliente(url: &str) -> AnthropicClient {
        AnthropicClient::new(url, ("x-api-key".into(), "sk-test".into()), &LlmConfig { model: "claude-opus-5-5".into(), effort: "high".into(), max_tokens: 1234, ..Default::default() })
    }

    #[test]
    fn la_peticion_http_lleva_lo_que_pide_la_api() {
        let ok = json!({"content": [{"type": "thinking", "thinking": "..."}, {"type": "text", "text": "{\"clips\": []}"}], "usage": {"input_tokens": 5, "output_tokens": 2}});
        let (url, recibidas) = servidor(vec![(200, ok.to_string())]);
        let respuesta = cliente(&url).complete("sistema", "usuario", &json!({"type": "object"})).unwrap();
        assert_eq!(respuesta, json!({"clips": []}));

        let (cabecera, cuerpo) = recibidas.lock().unwrap()[0].clone();
        let cabecera = cabecera.to_lowercase();
        assert!(cabecera.starts_with("post /v1/messages "), "{cabecera}");
        assert!(cabecera.contains("x-api-key: sk-test") && cabecera.contains("anthropic-version: 2023-06-01"), "{cabecera}");
        let cuerpo: Value = serde_json::from_str(&cuerpo).unwrap();
        assert_eq!(cuerpo["model"], "claude-opus-5-5");
        assert_eq!(cuerpo["max_tokens"], 1234);
        assert_eq!(cuerpo["system"], "sistema");
        assert_eq!(cuerpo["thinking"], json!({"type": "adaptive"}));
        assert_eq!(cuerpo["output_config"]["effort"], "high");
        assert_eq!(cuerpo["output_config"]["format"], json!({"type": "json_schema", "schema": {"type": "object"}}));
        assert_eq!(cuerpo["messages"], json!([{"role": "user", "content": "usuario"}]));
    }

    #[test]
    fn los_errores_de_la_api_llevan_codigo_y_mensaje() {
        let sobrecarga = json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}});
        let (url, _) = servidor(vec![(529, sobrecarga.to_string())]);
        let error = cliente(&url).complete("s", "u", &json!({})).unwrap_err().0;
        assert!(error.contains("529") && error.contains("Overloaded"), "{error}");
        assert!(is_retryable(&error));

        let (url, _) = servidor(vec![(401, json!({"error": {"message": "invalid x-api-key"}}).to_string())]);
        let error = cliente(&url).complete("s", "u", &json!({})).unwrap_err().0;
        assert!(error.contains("401") && !is_retryable(&error), "{error}");
    }

    #[test]
    fn una_negativa_o_un_contenido_vacio_son_errores_claros() {
        let (url, _) = servidor(vec![(200, json!({"stop_reason": "refusal", "stop_details": {"explanation": "no puedo"}, "content": []}).to_string())]);
        assert!(cliente(&url).complete("s", "u", &json!({})).unwrap_err().0.contains("declino") );

        let (url, _) = servidor(vec![(200, json!({"content": [{"type": "thinking"}]}).to_string())]);
        assert!(cliente(&url).complete("s", "u", &json!({})).unwrap_err().0.contains("contenido de texto"));

        let (url, _) = servidor(vec![(200, json!({"content": [{"type": "text", "text": "no es json"}]}).to_string())]);
        assert!(cliente(&url).complete("s", "u", &json!({})).unwrap_err().0.contains("JSON"));
    }

    #[test]
    fn sin_servidor_el_error_es_de_conexion_y_se_reintenta() {
        let error = cliente("http://127.0.0.1:9").complete("s", "u", &json!({})).unwrap_err().0;
        assert!(error.to_lowercase().contains("connection") && is_retryable(&error), "{error}");
    }

    #[test]
    fn las_credenciales_salen_del_entorno() {
        // Las variables de entorno son globales: todo en una sola prueba.
        let guardadas: Vec<_> = ["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_BASE_URL"].iter().map(|k| (*k, std::env::var(k).ok())).collect();
        for (k, _) in &guardadas {
            std::env::remove_var(k);
        }
        let cfg = LlmConfig::default();
        assert!(AnthropicClient::from_env(&cfg).err().unwrap().contains("ANTHROPIC_API_KEY"));

        std::env::set_var("ANTHROPIC_AUTH_TOKEN", "tok");
        assert_eq!(AnthropicClient::from_env(&cfg).ok().unwrap().auth, ("authorization".to_string(), "Bearer tok".to_string()));

        std::env::set_var("ANTHROPIC_API_KEY", "clave");
        std::env::set_var("ANTHROPIC_BASE_URL", "http://proxy.local/");
        let c = AnthropicClient::from_env(&cfg).ok().unwrap();
        assert_eq!(c.auth, ("x-api-key".to_string(), "clave".to_string()));
        assert_eq!(c.base_url, "http://proxy.local");

        for (k, v) in guardadas {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
    }
}
