//! Interfaz visual de viralshorts.
//!
//! Levanta un servidor local y abre el navegador. Se eligio navegador en vez de
//! una ventana de escritorio por una razon concreta: asi se pueden VER los
//! shorts generados dentro de la propia interfaz, que es justo lo que se quiere
//! comprobar. El dialogo para elegir ficheros si es nativo, porque el navegador
//! no puede darle a la pagina la ruta real de un fichero local.
//!
//! El trabajo va por una cola: se pueden dejar preparados varios podcast y se
//! montan uno detras de otro. Y hay dos formas de lanzarlo:
//!
//!   - directa:  buscar los momentos y montar los shorts del tiron.
//!   - revisada: buscar los momentos, verlos, quitar los que no valgan, corregir
//!               titulos y ganchos, y montar solo los que se han marcado.
//!
//! Es el mismo `web/index.html` de siempre y las mismas rutas `/api/*`. Cada
//! trabajo se lanza como un proceso aparte (este mismo ejecutable) para poder
//! pararlo entero, ffmpeg incluido.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

use crate::config::Config;
use crate::ffmpeg;
use crate::pipeline;
use crate::plan;
use crate::{fonts, library, progress, setup, update};

pub const DEFAULT_PORT: u16 = 8760;
const INDEX_HTML: &str = include_str!("../../web/index.html");
const SETUP_HTML: &str = include_str!("../../web/setup.html");
const EVENT_PREFIX: &str = "@@VS ";

/// Nombres con los que el navegador puede llegar legitimamente a este servidor.
/// Cualquier otro (un dominio atacante que resuelva a 127.0.0.1: DNS rebinding)
/// se rechaza antes de mirar nada mas.
const ALLOWED_HOSTS: [&str; 3] = ["127.0.0.1", "localhost", "[::1]"];

pub fn host_allowed(header: &str) -> bool {
    let host = header.trim().to_lowercase();
    let name = if host.starts_with('[') {
        host.split(']').next().map(|h| format!("{h}]")).unwrap_or_default()
    } else {
        host.rsplit_once(':').map_or(host.clone(), |(name, _)| name.to_string())
    };
    ALLOWED_HOSTS.contains(&name.as_str())
}

// ----------------------------------------------------------------------------
// Ajustes que la interfaz puede tocar
// ----------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Kind {
    Int,
    Ratio,
    Color,
    Text,
    Browser,
    /// Un modelo de voz de la lista (o vacio).
    Model,
    /// Texto libre y largo (vocabulario, correcciones), sin saltos de linea.
    Long,
}

/// La lista es cerrada a proposito: el navegador no decide que ajuste del
/// pipeline se cambia, solo elige entre estos.
const STYLE_FIELDS: [(&str, &str, Kind); 13] = [
    ("font_name", "subtitles.font_name", Kind::Text),
    ("font_size", "subtitles.font_size", Kind::Int),
    ("color_active", "subtitles.color_active", Kind::Color),
    ("position", "subtitles.vertical_position_ratio", Kind::Ratio),
    ("words", "subtitles.max_words_per_group", Kind::Int),
    ("hook_size", "hook.font_size", Kind::Int),
    ("max_height", "download.max_height", Kind::Int),
    ("workers", "output.render_workers", Kind::Int),
    ("encoder", "output.hardware_encoder", Kind::Text),
    ("cookies", "download.cookies_from_browser", Kind::Browser),
    ("refine", "transcribe.refine_model", Kind::Model),
    ("vocab", "transcribe.initial_prompt", Kind::Long),
    ("corrections", "transcribe.corrections", Kind::Long),
];

/// Navegadores de los que yt-dlp sabe leer las cookies.
const BROWSERS: [&str; 7] = ["chrome", "edge", "firefox", "brave", "opera", "vivaldi", "chromium"];

/// (ajuste, valor) del estilo de subtitulos que venga del formulario.
pub fn style_pairs(form: &Value) -> Vec<(String, Value)> {
    let style = &form["style"];
    let mut pairs = Vec::new();
    for (key, setting, kind) in STYLE_FIELDS {
        let raw = &style[key];
        if raw.is_null() || raw.as_str() == Some("") {
            continue;
        }
        let number = || match raw {
            Value::Number(n) => n.as_f64(),
            Value::String(s) => s.trim().parse::<f64>().ok(),
            _ => None,
        };
        let value = match kind {
            Kind::Int => number().map(|n| json!(n.trunc() as i64)),
            Kind::Ratio => number().map(|n| json!((n.clamp(0.0, 1.0) * 1000.0).round() / 1000.0)),
            Kind::Color => {
                let text = raw.as_str().unwrap_or("").trim();
                let valid = text.len() == 7 && text.starts_with('#') && text[1..].chars().all(|c| c.is_ascii_hexdigit());
                valid.then(|| json!(text.to_uppercase()))
            }
            Kind::Text => Some(json!(raw.as_str().unwrap_or("").trim().chars().take(60).collect::<String>())),
            Kind::Model => {
                let name = raw.as_str().unwrap_or("").trim();
                (name.is_empty() || setup::MODELS.iter().any(|(m, _)| *m == name)).then(|| json!(name))
            }
            Kind::Long => Some(json!(raw.as_str().unwrap_or("").chars().map(|c| if c == '\n' || c == '\r' { ';' } else { c }).filter(|c| !c.is_control()).take(600).collect::<String>().trim())),
            Kind::Browser => raw.as_str().map(|b| b.trim().to_lowercase()).filter(|b| BROWSERS.contains(&b.as_str())).map(|b| json!(b)),
        };
        if let Some(value) = value {
            pairs.push((setting.to_string(), value));
        }
    }
    pairs
}

fn value_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn form_number(form: &Value, key: &str) -> Option<f64> {
    match &form[key] {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}

/// Argumentos del pipeline para buscar (y montar) los clips.
pub fn build_command(form: &Value) -> Vec<String> {
    let text = |key: &str| form[key].as_str().unwrap_or("").to_string();
    let mut args = vec!["--podcast".to_string(), text("podcast"), "--background".into(), text("background")];

    if truthy(&form["config"]) {
        args.extend(["--config".into(), text("config")]);
    }
    for (flag, key, integer) in [
        ("--clips", "clips", true),
        ("--min-duration", "min_duration", false),
        ("--max-duration", "max_duration", false),
        ("--beam", "beam", true),
        ("--regions", "regions", true),
    ] {
        if truthy(&form[key]) {
            if let Some(n) = form_number(form, key) {
                args.extend([flag.to_string(), if integer { (n.trunc() as i64).to_string() } else { format!("{n:?}") }]);
            }
        }
    }
    for (flag, key) in [("--head-skip", "head_skip"), ("--tail-skip", "tail_skip")] {
        if !form[key].is_null() && form[key].as_str() != Some("") {
            if let Some(n) = form_number(form, key) {
                args.extend([flag.to_string(), format!("{n:?}")]);
            }
        }
    }
    for (flag, key) in [("--output", "output"), ("--model", "model"), ("--language", "language")] {
        if truthy(&form[key]) {
            args.extend([flag.to_string(), text(key)]);
        }
    }

    for (flag, key) in [
        ("--no-llm", "llm"),
        ("--no-reframe", "reframe"),
        ("--no-zoom", "zoom"),
        ("--no-jumpcuts", "jumpcuts"),
        ("--full-transcript", "prescan"),
        ("--no-title-card", "title_card"),
        ("--no-context", "context"),
        ("--no-censor", "censor"),
    ] {
        // Un interruptor ausente cuenta como encendido.
        let on = if form.get(key).is_none() { true } else { truthy(&form[key]) };
        if !on {
            args.push(flag.to_string());
        }
    }

    for (setting, value) in style_pairs(form) {
        args.extend(["--set".to_string(), format!("{setting}={}", value_text(&value))]);
    }

    if truthy(&form["plan_out"]) {
        args.extend(["--plan-out".into(), text("plan_out")]);
        if truthy(&form["propose"]) {
            if let Some(n) = form_number(form, "propose") {
                args.extend(["--propose".into(), (n.trunc() as i64).to_string()]);
            }
        }
    } else if truthy(&form["dry_run"]) {
        args.push("--dry-run".into());
    }
    args
}

/// Montar los clips marcados de un plan ya revisado.
pub fn plan_command(form: &Value) -> Vec<String> {
    let mut args = vec!["--from-plan".to_string(), value_text(&form["from_plan"])];
    if let Some(only) = form["only"].as_array().filter(|o| !o.is_empty()) {
        let numbers: Vec<String> = only.iter().filter_map(|n| n.as_f64()).map(|n| (n.trunc() as i64).to_string()).collect();
        args.extend(["--only".into(), numbers.join(",")]);
    }
    if truthy(&form["output"]) {
        args.extend(["--output".into(), value_text(&form["output"])]);
    }
    args
}

// ----------------------------------------------------------------------------
// Procesos
// ----------------------------------------------------------------------------

/// Un trabajo en marcha. Es un trait para probar la cola sin lanzar nada.
pub trait Process: Send {
    /// Llama a `on_line` con cada linea que el proceso escribe por stderr.
    fn read_lines(&mut self, on_line: &mut dyn FnMut(String));
    /// Espera a que termine y devuelve el codigo de salida.
    fn wait(&mut self) -> i32;
    /// Como pararlo desde otro hilo, con todo lo que haya lanzado.
    fn killer(&self) -> Box<dyn Fn() + Send + Sync>;
}

pub type Spawner = Arc<dyn Fn(&[String], &Path) -> std::io::Result<Box<dyn Process>> + Send + Sync>;

struct ChildProcess {
    child: std::process::Child,
}

impl Process for ChildProcess {
    fn read_lines(&mut self, on_line: &mut dyn FnMut(String)) {
        if let Some(stderr) = self.child.stderr.take() {
            for line in BufReader::new(stderr).split(b'\n').map_while(Result::ok) {
                on_line(String::from_utf8_lossy(&line).trim_end_matches('\r').to_string());
            }
        }
    }

    fn wait(&mut self) -> i32 {
        self.child.wait().ok().and_then(|s| s.code()).unwrap_or(-1)
    }

    fn killer(&self) -> Box<dyn Fn() + Send + Sync> {
        let pid = self.child.id();
        Box::new(move || kill_tree(pid))
    }
}

/// Mata el proceso y todos sus hijos (ffmpeg incluido). En Windows `/T` es la
/// unica forma fiable: no hay grupos de procesos que se puedan senalar.
fn kill_tree(pid: u32) {
    if cfg!(windows) {
        let _ = ffmpeg::command("taskkill").args(["/F", "/T", "/PID", &pid.to_string()]).output();
    } else {
        let _ = Command::new("kill").args(["-TERM", &format!("-{pid}")]).output();
        let _ = Command::new("kill").args(["-TERM", &pid.to_string()]).output();
    }
}

/// Este mismo ejecutable, con los argumentos del pipeline.
pub fn real_spawner() -> Spawner {
    Arc::new(|args, cwd| {
        let exe = std::env::current_exe()?;
        let child = ffmpeg::command(exe)
            .args(args)
            .current_dir(cwd)
            .env("VIRALSHORTS_PROGRESS", "json")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()?;
        Ok(Box::new(ChildProcess { child }) as Box<dyn Process>)
    })
}

// ----------------------------------------------------------------------------
// Estado del trabajo en curso
// ----------------------------------------------------------------------------

struct State {
    status: String, // idle | running | done | error | stopped
    stage: String,
    log: Vec<String>,
    clips: Vec<Value>,
    results: Vec<Value>,
    notices: Vec<Value>,
    /// Ultimas frases transcritas, para quien abre la ventana a mitad.
    live: Vec<Value>,
    output_dir: PathBuf,
    /// Carpeta de la que cuelgan las de cada ejecucion: la biblioteca la recorre.
    base_dir: PathBuf,
    plan_path: Option<PathBuf>,
    plan_clips: Vec<Value>,
    pending: Vec<Value>,
    killer: Option<Box<dyn Fn() + Send + Sync>>,
}

/// La ejecucion en marcha, mas la cola de las que esperan.
pub struct Job {
    root: PathBuf,
    spawner: Spawner,
    state: Mutex<State>,
    subscribers: Mutex<Vec<Sender<Value>>>,
    /// Pausa entre un trabajo y el siguiente de la cola, para que el cerrojo
    /// de work/ se suelte.
    handoff: Duration,
}

impl Job {
    pub fn new(root: &Path, spawner: Spawner) -> Arc<Self> {
        Self::with_handoff(root, spawner, Duration::from_millis(400))
    }

    pub fn with_handoff(root: &Path, spawner: Spawner, handoff: Duration) -> Arc<Self> {
        let mut state = empty_state();
        state.output_dir = latest_run_dir(&root.join("output"));
        state.base_dir = configured_output_dir(root);
        Arc::new(Self { root: root.to_path_buf(), spawner, state: Mutex::new(state), subscribers: Mutex::new(Vec::new()), handoff })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    // -- difusion de eventos ----------------------------------------------------
    pub fn subscribe(&self) -> Receiver<Value> {
        let (tx, rx) = mpsc::channel();
        self.subscribers.lock().unwrap().push(tx);
        rx
    }

    pub fn publish(&self, event: Value) {
        // Un cliente que se fue se quita; uno lento no frena al pipeline.
        self.subscribers.lock().unwrap().retain(|tx| tx.send(event.clone()).is_ok());
    }

    pub fn snapshot(&self) -> Value {
        let state = self.lock();
        json!({
            "status": state.status,
            "stage": state.stage,
            "log": state.log[state.log.len().saturating_sub(400)..],
            "clips": state.clips,
            "results": state.results,
            "notices": state.notices,
            "live": state.live,
            "output_dir": state.output_dir.to_string_lossy(),
            "plan": plan_summary(&state),
            "queue": queue_summary(&state),
            "env": environment(&self.root),
        })
    }

    pub fn status(&self) -> String {
        self.lock().status.clone()
    }

    pub fn output_dir(&self) -> PathBuf {
        self.lock().output_dir.clone()
    }

    pub fn base_dir(&self) -> PathBuf {
        self.lock().base_dir.clone()
    }

    pub fn set_base_dir(&self, dir: PathBuf) {
        self.lock().base_dir = dir;
    }

    pub fn set_output_dir(&self, dir: PathBuf) {
        self.lock().output_dir = dir;
    }

    pub fn plan_path(&self) -> Option<PathBuf> {
        self.lock().plan_path.clone()
    }

    pub fn plan_summary(&self) -> Option<Value> {
        plan_summary(&self.lock())
    }

    pub fn set_plan(&self, path: PathBuf, clips: Vec<Value>) {
        let mut state = self.lock();
        state.plan_path = Some(path.clone());
        state.plan_clips = clips.clone();
        drop(state);
        self.publish(json!({"type": "plan", "ruta": path.to_string_lossy(), "clips": clips}));
    }

    pub fn pending_len(&self) -> usize {
        self.lock().pending.len()
    }

    /// Quita de la cola el trabajo `n` (1 = el siguiente en salir).
    pub fn remove_pending(&self, n: usize) -> bool {
        let mut state = self.lock();
        if n == 0 || n > state.pending.len() {
            return false;
        }
        state.pending.remove(n - 1);
        let queue = queue_summary(&state);
        drop(state);
        self.publish(json!({"type": "queue", "queue": queue}));
        true
    }

    /// Adelanta (`delta` < 0) o atrasa (`delta` > 0) el trabajo `n` de la cola.
    pub fn move_pending(&self, n: usize, delta: i64) -> bool {
        let mut state = self.lock();
        let len = state.pending.len();
        let target = n as i64 - 1 + delta.signum();
        if n == 0 || n > len || target < 0 || target >= len as i64 {
            return false;
        }
        state.pending.swap(n - 1, target as usize);
        let queue = queue_summary(&state);
        drop(state);
        self.publish(json!({"type": "queue", "queue": queue}));
        true
    }

    /// Para solo el trabajo en marcha; lo que espera en la cola sigue su curso.
    pub fn stop_current(&self) {
        let killer = {
            let mut state = self.lock();
            if state.status == "running" {
                state.status = "stopped".into();
                state.killer.take()
            } else {
                None
            }
        };
        if let Some(kill) = killer {
            kill();
            let mut state = self.lock();
            self.push_log(&mut state, "-- saltado por el usuario --".into());
        }
    }

    pub fn clear_queue(&self) {
        self.lock().pending.clear();
        self.publish(json!({"type": "queue", "queue": []}));
    }

    // -- ciclo de vida --------------------------------------------------------------
    /// Lanza el trabajo, o lo deja en la cola si hay otro en marcha. Decidir
    /// "arranco o encolo" y pasar al siguiente se hace con el mismo cerrojo: si
    /// entre las dos cosas se colara otra peticion, vería el estado libre y
    /// lanzaria su trabajo a la vez que el que sale de la cola.
    pub fn enqueue(self: &Arc<Self>, form: Value) -> Value {
        let mut state = self.lock();
        if state.status == "running" {
            state.pending.push(form);
            let queue = queue_summary(&state);
            let position = state.pending.len();
            drop(state);
            self.publish(json!({"type": "queue", "queue": queue}));
            return json!({"ok": true, "encolado": true, "posicion": position});
        }
        self.start(&mut state, form);
        json!({"ok": true, "encolado": false})
    }

    fn start(self: &Arc<Self>, state: &mut State, form: Value) {
        state.status = "running".into();
        state.stage = "preparando".into();
        state.log.clear();
        state.clips.clear();
        state.results.clear();
        state.notices.clear();
        state.live.clear();
        state.base_dir = match form["output"].as_str().filter(|o| !o.is_empty()) {
            Some(output) => {
                let output = PathBuf::from(output);
                if output.is_absolute() { output } else { self.root.join(output) }
            }
            None => configured_output_dir(&self.root),
        };
        state.output_dir = state.base_dir.clone();

        let args = if truthy(&form["from_plan"]) {
            plan_command(&form)
        } else {
            // Un plan nuevo invalida el anterior: los clips que se ven en
            // pantalla tienen que ser los de esta ejecucion.
            state.plan_path = form["plan_out"].as_str().filter(|p| !p.is_empty()).map(PathBuf::from);
            state.plan_clips.clear();
            build_command(&form)
        };

        let queue = queue_summary(state);
        self.publish(json!({"type": "job", "status": "running", "queue": queue}));
        self.push_log(state, format!("$ {}", args.join(" ")));

        match (self.spawner)(&args, &self.root) {
            Ok(process) => {
                state.killer = Some(process.killer());
                let job = self.clone();
                std::thread::spawn(move || job.pump(process));
            }
            Err(error) => {
                // Sin esto el estado se quedaba en "running" para siempre y la
                // cola no volvia a arrancar nada.
                state.status = "error".into();
                self.push_log(state, format!("No se pudo lanzar el pipeline: {error}"));
                let queue = queue_summary(state);
                self.publish(json!({"type": "job", "status": "error", "code": Value::Null, "queue": queue}));
            }
        }
    }

    /// Para lo que haya en marcha, con todo lo que haya lanzado.
    pub fn stop(&self) {
        let killer = {
            let mut state = self.lock();
            if !state.pending.is_empty() {
                state.pending.clear();
                drop(state);
                self.publish(json!({"type": "queue", "queue": []}));
                state = self.lock();
            }
            if state.status == "running" {
                state.status = "stopped".into();
                state.killer.take()
            } else {
                None
            }
        };
        if let Some(kill) = killer {
            kill();
            let mut state = self.lock();
            self.push_log(&mut state, "-- detenido por el usuario --".into());
        }
    }

    /// Lee stderr del pipeline y lo reparte a los navegadores conectados.
    fn pump(self: Arc<Self>, mut process: Box<dyn Process>) {
        process.read_lines(&mut |line| {
            if let Some(payload) = line.strip_prefix(EVENT_PREFIX) {
                self.handle_event(payload);
            } else if !line.trim().is_empty() {
                let mut state = self.lock();
                self.push_log(&mut state, line);
            }
        });
        let code = process.wait();

        // Todo el relevo bajo el cerrojo, como se explica en `enqueue`.
        let mut state = self.lock();
        if state.status != "stopped" {
            state.status = if code == 0 { "done".into() } else { "error".into() };
        }
        state.killer = None;
        let (status, queue) = (state.status.clone(), queue_summary(&state));
        self.publish(json!({"type": "job", "status": status, "code": code, "queue": queue}));

        // Siguiente de la cola. Si el usuario ha pulsado Detener, la cola ya
        // esta vacia y aqui no arranca nada.
        if !state.pending.is_empty() {
            std::thread::sleep(self.handoff);
            let next = state.pending.remove(0);
            self.start(&mut state, next);
        }
    }

    fn handle_event(&self, payload: &str) {
        let Ok(event) = serde_json::from_str::<Value>(payload) else { return };
        {
            let mut state = self.lock();
            match event["type"].as_str().unwrap_or("") {
                "stage" => {
                    state.stage = event["label"].as_str().filter(|l| !l.is_empty()).or(event["stage"].as_str()).unwrap_or("").to_string();
                }
                "clips" => state.clips = event["clips"].as_array().cloned().unwrap_or_default(),
                "clip_done" => state.results.push(event.clone()),
                "aviso" => state.notices.push(event.clone()),
                "texto" => {
                    state.live.push(event.clone());
                    if state.live.len() > 40 {
                        state.live.remove(0);
                    }
                }
                // Carpeta de esta ejecucion: de ahi salen los videos que se ven.
                "salida" => {
                    if let Some(ruta) = event["ruta"].as_str() {
                        state.output_dir = PathBuf::from(ruta);
                    }
                }
                "plan" => {
                    state.plan_path = event["ruta"].as_str().map(PathBuf::from);
                    state.plan_clips = event["clips"].as_array().cloned().unwrap_or_default();
                }
                _ => {}
            }
        }
        self.publish(event);
    }

    fn push_log(&self, state: &mut State, line: String) {
        state.log.push(line.clone());
        if state.log.len() > 2000 {
            state.log.drain(..500);
        }
        self.publish(json!({"type": "log", "line": line}));
    }
}

fn empty_state() -> State {
    State {
        status: "idle".into(),
        stage: String::new(),
        log: Vec::new(),
        clips: Vec::new(),
        results: Vec::new(),
        notices: Vec::new(),
        live: Vec::new(),
        output_dir: PathBuf::from("output"),
        base_dir: PathBuf::from("output"),
        plan_path: None,
        plan_clips: Vec::new(),
        pending: Vec::new(),
        killer: None,
    }
}

fn plan_summary(state: &State) -> Option<Value> {
    state.plan_path.as_ref().map(|p| json!({"ruta": p.to_string_lossy(), "clips": state.plan_clips}))
}

fn queue_summary(state: &State) -> Value {
    Value::Array(
        state
            .pending
            .iter()
            .enumerate()
            .map(|(i, item)| {
                json!({
                    "n": i + 1,
                    "podcast": item["podcast"].as_str().unwrap_or(""),
                    "clips": item["clips"].as_i64().unwrap_or(0),
                    "revisar": truthy(&item["review"]),
                })
            })
            .collect(),
    )
}

// ----------------------------------------------------------------------------
// Entorno y ficheros
// ----------------------------------------------------------------------------

fn has_shorts(folder: &Path) -> bool {
    std::fs::read_dir(folder).is_ok_and(|entries| {
        entries.flatten().any(|e| {
            let name = e.file_name().to_string_lossy().to_lowercase();
            name.starts_with("short_") && name.ends_with(".mp4")
        })
    })
}

/// Carpeta de la ultima ejecucion con shorts (o `base` si no hay ninguna).
pub fn latest_run_dir(base: &Path) -> PathBuf {
    if has_shorts(base) || !base.is_dir() {
        return base.to_path_buf();
    }
    let newest = std::fs::read_dir(base)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir() && has_shorts(p))
        .max_by_key(|p| p.metadata().and_then(|m| m.modified()).ok());
    newest.unwrap_or_else(|| base.to_path_buf())
}

/// Carpeta de salida de `config.yaml` (o `output` junto al programa).
pub fn configured_output_dir(root: &Path) -> PathBuf {
    let config = root.join("config.yaml");
    let dir = Config::load(config.is_file().then_some(config.as_path())).map(|c| c.output.dir).unwrap_or_else(|_| "output".into());
    let dir = PathBuf::from(if dir.is_empty() { "output".to_string() } else { dir });
    if dir.is_absolute() { dir } else { root.join(dir) }
}

/// Lo que la interfaz necesita para avisar ANTES de fallar a mitad. Enterarse de
/// que falta ffmpeg, la clave de Claude o la fuente de los subtitulos cuando
/// llevas veinte minutos transcribiendo es la peor forma posible de enterarse.
pub fn environment(root: &Path) -> Value {
    let config = root.join("config.yaml");
    let font = Config::load(config.is_file().then_some(config.as_path())).map(|c| c.subtitles.font_name).unwrap_or_else(|_| "Arial Black".into());
    let font_ok = fonts::is_available(&font, None);
    let has_key = ["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN"].iter().any(|k| std::env::var(k).is_ok_and(|v| !v.is_empty()));
    json!({
        "ffmpeg": ffmpeg::which("ffmpeg").is_some(),
        "has_api_key": has_key,
        "font": font,
        "font_ok": font_ok,
        "font_alternative": if font_ok { String::new() } else { fonts::suggest_alternative(&font) },
        "version": update::CURRENT,
        "nvidia": nvidia_present(),
        "cuda_engine": setup::cuda_installed(root),
        "output_dir": configured_output_dir(root).to_string_lossy(),
    })
}

/// Hay una tarjeta NVIDIA. Se pregunta una sola vez: `environment` se calcula
/// en cada estado que se manda a la interfaz.
pub fn nvidia_present() -> bool {
    static PRESENT: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *PRESENT.get_or_init(|| {
        ffmpeg::which("nvidia-smi").is_some_and(|smi| {
            ffmpeg::command(smi).arg("-L").stdin(Stdio::null()).output().is_ok_and(|o| o.status.success() && !o.stdout.is_empty())
        })
    })
}

/// Shorts ya generados en la carpeta de salida, con sus metadatos.
pub fn scan_output(folder: &Path) -> Vec<Value> {
    let Ok(entries) = std::fs::read_dir(folder) else { return Vec::new() };
    let mut videos: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            let name = p.file_name().unwrap_or_default().to_string_lossy().to_lowercase();
            name.starts_with("short_") && name.ends_with(".mp4")
        })
        .collect();
    videos.sort();

    videos
        .into_iter()
        .map(|video| {
            let mut entry = json!({
                "fichero": video.file_name().unwrap_or_default().to_string_lossy(),
                "ruta": video.to_string_lossy(),
                "titulo": "", "gancho": "", "pie": "",
                "mtime": video.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs()),
            });
            if let Some(data) = std::fs::read_to_string(video.with_extension("json")).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()) {
                let publicacion = &data["publicacion"];
                let tags: Vec<String> = publicacion["hashtags"].as_array().map(|t| t.iter().filter_map(Value::as_str).map(|t| format!("#{t}")).collect()).unwrap_or_default();
                let description = publicacion["description"].as_str().unwrap_or("");
                let caption = if tags.is_empty() { description.to_string() } else { format!("{description}\n\n{}", tags.join(" ")) };
                entry["titulo"] = json!(publicacion["title"].as_str().unwrap_or(""));
                entry["gancho"] = json!(publicacion["hook"].as_str().unwrap_or(""));
                entry["pie"] = json!(caption.trim());
                entry["marca"] = json!(data["fuente"]["marca"].as_str().unwrap_or(""));
            }
            entry
        })
        .collect()
}

/// Titulo y duracion de una URL, SIN descargar el video. Sirve para que la
/// interfaz confirme que lo pegado es lo que se cree, antes de gastar veinte
/// minutos en el video equivocado.
pub fn probe_title(url: &str, cache: &Mutex<HashMap<String, Value>>) -> Value {
    let url = url.trim();
    if url.is_empty() {
        return json!({"ok": false, "error": "sin url"});
    }
    if let Some(hit) = cache.lock().unwrap().get(url) {
        return hit.clone();
    }

    let local = Path::new(url);
    let answer = if local.is_file() {
        json!({"ok": true, "title": local.file_stem().unwrap_or_default().to_string_lossy(), "source": "archivo"})
    } else if let Some(exe) = ffmpeg::which("yt-dlp") {
        let output = ffmpeg::command(exe)
            .args(["--ignore-config", "--no-warnings", "--no-playlist", "--skip-download", "--flat-playlist", "--dump-single-json", "--", url])
            .stdin(Stdio::null())
            .output();
        match output {
            Ok(out) if out.status.success() => match serde_json::from_slice::<Value>(&out.stdout) {
                Ok(mut info) => {
                    if info["_type"] == "playlist" {
                        info = info["entries"].as_array().and_then(|e| e.iter().find(|x| !x.is_null()).cloned()).unwrap_or(Value::Null);
                    }
                    json!({
                        "ok": true,
                        "title": info["title"].as_str().unwrap_or(""),
                        "uploader": info["uploader"].as_str().or(info["channel"].as_str()).unwrap_or(""),
                        "duration": info["duration"].as_f64().unwrap_or(0.0),
                        "source": "youtube",
                    })
                }
                Err(e) => json!({"ok": false, "error": e.to_string()}),
            },
            // No es un error del que haya que avisar: la interfaz simplemente
            // se queda sin titulo y sigue mostrando el enlace.
            Ok(out) => json!({"ok": false, "error": String::from_utf8_lossy(&out.stderr).chars().take(200).collect::<String>()}),
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    } else {
        return json!({"ok": false, "error": "falta yt-dlp"});
    };

    cache.lock().unwrap().insert(url.to_string(), answer.clone());
    answer
}

fn new_plan_path(root: &Path) -> PathBuf {
    let folder = root.join("work").join("plans");
    let _ = std::fs::create_dir_all(&folder);
    folder.join(format!("plan_{}.json", chrono::Local::now().format("%Y%m%d_%H%M%S")))
}

// ----------------------------------------------------------------------------
// Aplicacion y servidor HTTP
// ----------------------------------------------------------------------------

pub struct App {
    pub token: String,
    pub root: PathBuf,
    pub job: Arc<Job>,
    titles: Mutex<HashMap<String, Value>>,
    /// Dialogo de ficheros del sistema. None = no disponible (y en pruebas).
    picker: Option<Box<dyn Fn(&str) -> Option<String> + Send + Sync>>,
    /// Instalacion de las herramientas (primer arranque, motor de GPU).
    setup: Arc<setup::Shared>,
    /// Con true, mientras falten herramientas la pagina principal es la de
    /// instalacion. En las pruebas va apagado.
    setup_required: bool,
    /// Actualizaciones: estado, donde mirar, y como lanzar el instalador y
    /// cerrar la aplicacion (inyectables para las pruebas).
    updates: Arc<update::Shared>,
    update_url: String,
    run_installer: Arc<dyn Fn(&Path) -> std::io::Result<()> + Send + Sync>,
    quit: Arc<dyn Fn() + Send + Sync>,
}

impl App {
    pub fn new(root: &Path, spawner: Spawner) -> Arc<Self> {
        let job = Job::new(root, spawner);
        let job_to_stop = job.clone();
        Arc::new(Self {
            token: new_token(),
            root: root.to_path_buf(),
            job,
            titles: Mutex::new(HashMap::new()),
            picker: Some(Box::new(native_picker)),
            setup: Arc::new(Mutex::new(setup::State::default())),
            setup_required: true,
            updates: Arc::new(Mutex::new(update::State::default())),
            update_url: update::manifest_url(),
            run_installer: Arc::new(update::launch_installer),
            // Un trabajo a medias no se queda huerfano al cerrar para actualizar.
            quit: Arc::new(move || {
                job_to_stop.stop();
                std::process::exit(0);
            }),
        })
    }

    pub fn with_job(root: &Path, job: Arc<Job>, token: &str) -> Arc<Self> {
        Arc::new(Self {
            token: token.to_string(),
            root: root.to_path_buf(),
            job,
            titles: Mutex::new(HashMap::new()),
            picker: None,
            setup: Arc::new(Mutex::new(setup::State::default())),
            setup_required: false,
            updates: Arc::new(Mutex::new(update::State::default())),
            update_url: String::new(),
            run_installer: Arc::new(|_| Ok(())),
            quit: Arc::new(|| {}),
        })
    }

    /// Mira en segundo plano si hay una version nueva.
    fn start_update_check(&self) {
        let (state, url) = (self.updates.clone(), self.update_url.clone());
        std::thread::spawn(move || update::run_check(&state, &url, update::CURRENT));
    }

    /// Baja e instala la version que se ofrece, si no hay un trabajo en marcha.
    fn start_update_install(&self) -> Result<(), String> {
        if self.job.status() == "running" {
            return Err("Hay un trabajo en marcha. Espera a que termine (o detenlo) y actualiza despues.".into());
        }
        let (state, launch, quit) = (self.updates.clone(), self.run_installer.clone(), self.quit.clone());
        std::thread::spawn(move || update::run_install(&state, &*launch, &*quit));
        Ok(())
    }

    /// Lanza la instalacion de lo que falte (o del motor de GPU) en un hilo.
    fn start_setup(&self, tools: Vec<setup::Tool>) {
        let (root, state) = (self.root.clone(), self.setup.clone());
        std::thread::spawn(move || setup::install(&root, &tools, &state));
    }
}

/// 16 bytes aleatorios en base64 de URL, como `secrets.token_urlsafe(16)`.
pub fn new_token() -> String {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("el sistema da bytes aleatorios");
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |acc, (i, b)| acc | (*b as u32) << (16 - 8 * i));
        let symbols = chunk.len() + 1;
        for i in 0..symbols {
            out.push(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize] as char);
        }
    }
    out
}

fn native_picker(kind: &str) -> Option<String> {
    let path = if kind == "folder" {
        rfd::FileDialog::new().set_title("Elige la carpeta de salida").pick_folder()
    } else {
        rfd::FileDialog::new().set_title("Elige un video").add_filter("Videos", &["mp4", "mkv", "webm", "mov", "m4v", "avi"]).add_filter("Todos", &["*"]).pick_file()
    };
    path.map(|p| p.to_string_lossy().into_owned())
}

struct Request {
    method: String,
    path: String,
    query: HashMap<String, String>,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

fn url_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => match ((bytes[i + 1] as char).to_digit(16), (bytes[i + 2] as char).to_digit(16)) {
                (Some(high), Some(low)) => {
                    out.push((high * 16 + low) as u8);
                    i += 2;
                }
                _ => out.push(b'%'),
            },
            other => out.push(other),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn read_request(stream: &mut TcpStream) -> Option<Request> {
    stream.set_read_timeout(Some(Duration::from_secs(30))).ok()?;
    let mut reader = BufReader::new(stream.try_clone().ok()?);

    let mut head = String::new();
    reader.read_line(&mut head).ok()?;
    let mut parts = head.split_whitespace();
    let (method, target) = (parts.next()?.to_string(), parts.next()?.to_string());

    let mut headers = HashMap::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if headers.len() > 100 {
            return None;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_lowercase(), value.trim().to_string());
        }
    }

    let length: usize = headers.get("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
    if length > 8 * 1024 * 1024 {
        return None;
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).ok()?;

    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path.to_string(), query.to_string()),
        None => (target.clone(), String::new()),
    };
    let query = query
        .split('&')
        .filter(|kv| !kv.is_empty())
        .filter_map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            Some((url_decode(k), url_decode(v)))
        })
        .fold(HashMap::new(), |mut map, (k, v)| {
            map.entry(k).or_insert(v);
            map
        });
    Some(Request { method, path, query, headers, body })
}

fn respond(stream: &mut TcpStream, status: u16, headers: &[(&str, String)], body: &[u8]) {
    let reason = match status {
        200 => "OK",
        206 => "Partial Content",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        416 => "Range Not Satisfiable",
        _ => "Error",
    };
    let mut head = format!("HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n", body.len());
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    let _ = stream.write_all(head.as_bytes()).and_then(|_| stream.write_all(body)).and_then(|_| stream.flush());
}

fn respond_json(stream: &mut TcpStream, status: u16, payload: &Value) {
    respond(stream, status, &[("Content-Type", "application/json; charset=utf-8".into()), ("Cache-Control", "no-store".into())], payload.to_string().as_bytes());
}

fn respond_error(stream: &mut TcpStream, status: u16, message: &str) {
    respond(stream, status, &[("Content-Type", "text/plain; charset=utf-8".into())], message.as_bytes());
}

/// Host de este equipo Y token, para TODO: paginas, datos y videos. Con solo el
/// token en las peticiones POST, una web atacante que resolviera su dominio a
/// 127.0.0.1 (DNS rebinding) podia pedir `/`, leer el token incrustado en la
/// pagina y despues lanzar procesos.
fn allowed(app: &App, request: &Request, stream: &mut TcpStream) -> bool {
    if !host_allowed(request.headers.get("host").map_or("", String::as_str)) {
        respond_error(stream, 403, "host no permitido");
        return false;
    }
    let expected = app.token.as_bytes();
    let given = [request.headers.get("x-vs-token").map(String::as_str), request.query.get("t").map(String::as_str)];
    if !given.iter().flatten().any(|g| constant_time_eq(g.as_bytes(), expected)) {
        respond_error(stream, 403, "falta el token: abre la direccion completa que sale en la consola");
        return false;
    }
    true
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub fn handle_connection(app: &Arc<App>, mut stream: TcpStream) {
    let Some(request) = read_request(&mut stream) else { return };
    if !allowed(app, &request, &mut stream) {
        return;
    }
    match request.method.as_str() {
        "GET" => handle_get(app, &request, stream),
        "POST" => handle_post(app, &request, &mut stream),
        _ => respond_error(&mut stream, 404, "no encontrado"),
    }
}

fn handle_get(app: &Arc<App>, request: &Request, mut stream: TcpStream) {
    match request.path.as_str() {
        "/" => {
            // Sin las herramientas no hay nada que hacer: primero se instalan.
            let page = if app.setup_required && !setup::missing(&app.root).is_empty() { SETUP_HTML } else { INDEX_HTML };
            respond(
                &mut stream,
                200,
                &[("Content-Type", "text/html; charset=utf-8".into()), ("Cache-Control", "no-store".into())],
                page.replace("__TOKEN__", &app.token).as_bytes(),
            )
        }
        "/api/models" => respond_json(&mut stream, 200, &setup::models_json(&app.root)),
        "/api/setup" => respond_json(&mut stream, 200, &app.setup.lock().unwrap_or_else(|e| e.into_inner()).to_json(&app.root)),
        "/api/update" => respond_json(&mut stream, 200, &app.updates.lock().unwrap_or_else(|e| e.into_inner()).to_json()),
        "/api/state" => respond_json(&mut stream, 200, &app.job.snapshot()),
        "/api/events" => serve_events(app, stream),
        "/api/video" => serve_file(app, request.query.get("f").map_or("", String::as_str), request, &mut stream),
        "/api/existing" => respond_json(&mut stream, 200, &json!({"results": library::scan_library(&app.job.base_dir())})),
        "/api/profiles" => respond_json(&mut stream, 200, &json!({"profiles": library::load_profiles(&app.root)})),
        "/api/frame" => serve_frame(request.query.get("src").map_or("", String::as_str), &mut stream),
        "/api/reveal" => reveal(app, request, &mut stream),
        _ => respond_error(&mut stream, 404, "no encontrado"),
    }
}

/// Un fotograma de un video o imagen local, para la vista previa.
fn serve_frame(source: &str, stream: &mut TcpStream) {
    match library::frame_jpeg(Path::new(source)) {
        Some(jpeg) => respond(stream, 200, &[("Content-Type", "image/jpeg".into()), ("Cache-Control", "private, max-age=300".into())], &jpeg),
        None => respond_error(stream, 404, "sin fotograma"),
    }
}

fn serve_events(app: &Arc<App>, mut stream: TcpStream) {
    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream; charset=utf-8\r\nCache-Control: no-store\r\nConnection: keep-alive\r\n\r\n";
    if stream.write_all(head.as_bytes()).is_err() {
        return;
    }
    let events = app.job.subscribe();
    let mut hello = app.job.snapshot();
    hello["type"] = json!("hello");
    if write_event(&mut stream, &hello).is_err() {
        return;
    }
    loop {
        let sent = match events.recv_timeout(Duration::from_secs(15)) {
            Ok(event) => write_event(&mut stream, &event),
            // Mantiene viva la conexion.
            Err(mpsc::RecvTimeoutError::Timeout) => stream.write_all(b": ping\n\n").and_then(|_| stream.flush()),
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        };
        if sent.is_err() {
            return; // el navegador se fue; el suscriptor se limpia solo
        }
    }
}

fn write_event(stream: &mut TcpStream, event: &Value) -> std::io::Result<()> {
    stream.write_all(format!("data: {event}\n\n").as_bytes())?;
    stream.flush()
}

/// Nombres de fichero que se pueden pedir: solo el nombre, de un video o una
/// portada. Nada de rutas.
pub fn valid_media_name(name: &str) -> bool {
    let Some((stem, ext)) = name.rsplit_once('.') else { return false };
    !stem.is_empty()
        && ["mp4", "mkv", "webm", "jpg"].contains(&ext)
        && name.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '.' || c == '-' || c == ' ')
}

/// Igual, pero admite el `carpeta/fichero` con el que la biblioteca nombra los
/// shorts de cada ejecucion.
fn valid_media_id(id: &str) -> bool {
    match id.split_once('/') {
        Some((folder, file)) => {
            !file.contains('/')
                && valid_media_name(file)
                && !folder.is_empty()
                && folder != "."
                && folder != ".."
                && folder.chars().all(|c| c.is_alphanumeric() || matches!(c, '_' | '.' | '-' | ' ' | '(' | ')'))
        }
        None => valid_media_name(id),
    }
}

/// Sirve un short (o su portada) de la carpeta de salida. Con soporte de Range:
/// sin el, el navegador no puede saltar por la barra de tiempo del video.
fn serve_file(app: &Arc<App>, name: &str, request: &Request, stream: &mut TcpStream) {
    if !valid_media_id(name) {
        return respond_error(stream, 400, "nombre invalido");
    }
    // La biblioteca pide `carpeta/fichero` desde la carpeta base; lo de la
    // ejecucion en curso, solo el nombre desde su carpeta.
    let candidates = [app.job.base_dir(), app.job.output_dir()];
    let found = candidates.iter().find_map(|folder| {
        let path = folder.join(name);
        Some((path.canonicalize().ok()?, folder.canonicalize().ok()?))
    });
    let Some((canonical, base)) = found else {
        return respond_error(stream, 404, "no encontrado");
    };
    if !canonical.starts_with(&base) {
        return respond_error(stream, 403, "prohibido");
    }
    let Ok(mut file) = std::fs::File::open(&canonical) else { return respond_error(stream, 404, "no encontrado") };
    let size = file.metadata().map_or(0, |m| m.len());
    let mime = match name.rsplit('.').next().unwrap_or("") {
        "jpg" => "image/jpeg",
        "webm" => "video/webm",
        "mkv" => "video/x-matroska",
        _ => "video/mp4",
    };

    let (mut start, mut end, mut status) = (0u64, size.saturating_sub(1), 200u16);
    if let Some(range) = request.headers.get("range").and_then(|r| r.strip_prefix("bytes=")) {
        let (first, last) = range.split_once('-').unwrap_or((range, ""));
        match (first.trim().parse::<u64>().ok(), last.trim().parse::<u64>().ok()) {
            (Some(a), b) => {
                start = a;
                end = b.map_or(end, |b| b.min(size.saturating_sub(1)));
            }
            (None, Some(n)) => start = size.saturating_sub(n), // los ultimos N bytes
            (None, None) => {}
        }
        if start >= size || start > end {
            return respond(stream, 416, &[("Content-Range", format!("bytes */{size}"))], b"");
        }
        status = 206;
    }

    let length = end - start + 1;
    let mut head = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: {mime}\r\nAccept-Ranges: bytes\r\nContent-Length: {length}\r\nConnection: close\r\n",
        if status == 206 { "Partial Content" } else { "OK" }
    );
    if status == 206 {
        head.push_str(&format!("Content-Range: bytes {start}-{end}/{size}\r\n"));
    }
    head.push_str("\r\n");
    if stream.write_all(head.as_bytes()).is_err() {
        return;
    }

    use std::io::Seek;
    if file.seek(std::io::SeekFrom::Start(start)).is_err() {
        return;
    }
    let mut remaining = length;
    let mut buffer = vec![0u8; 256 * 1024];
    while remaining > 0 {
        let want = buffer.len().min(remaining as usize);
        match file.read(&mut buffer[..want]) {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                if stream.write_all(&buffer[..read]).is_err() {
                    return;
                }
                remaining -= read as u64;
            }
        }
    }
}

fn reveal(app: &Arc<App>, request: &Request, stream: &mut TcpStream) {
    // Con `f` (un id de la biblioteca) se abre la carpeta de ese short.
    let from_id = request
        .query
        .get("f")
        .and_then(|id| library::resolve_short(&app.job.base_dir(), id))
        .and_then(|video| video.parent().map(Path::to_path_buf));
    let folder = from_id.unwrap_or_else(|| app.job.output_dir());
    let _ = std::fs::create_dir_all(&folder);
    let opener = if cfg!(windows) { "explorer" } else if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    match Command::new(opener).arg(&folder).spawn() {
        Ok(_) => respond_json(stream, 200, &json!({"ok": true})),
        Err(error) => respond_json(stream, 200, &json!({"ok": false, "error": error.to_string()})),
    }
}

fn handle_post(app: &Arc<App>, request: &Request, stream: &mut TcpStream) {
    let body: Value = if request.body.is_empty() { json!({}) } else {
        match serde_json::from_slice(&request.body) {
            Ok(body) => body,
            Err(error) => return respond_json(stream, 500, &json!({"ok": false, "error": error.to_string()})),
        }
    };

    let (status, answer) = match request.path.as_str() {
        "/api/run" => run_form(app, body),
        "/api/stop" => {
            if truthy(&body["solo_este"]) {
                app.job.stop_current();
            } else {
                app.job.stop();
            }
            (200, json!({"ok": true}))
        }
        "/api/queue/remove" => (200, json!({"ok": app.job.remove_pending(body["n"].as_u64().unwrap_or(0) as usize)})),
        "/api/queue/move" => (200, json!({"ok": app.job.move_pending(body["n"].as_u64().unwrap_or(0) as usize, body["delta"].as_i64().unwrap_or(0))})),
        "/api/short/delete" => match library::delete_short(&app.job.base_dir(), body["id"].as_str().unwrap_or("")) {
            Ok(removed) => (200, json!({"ok": true, "borrados": removed})),
            Err(error) => (400, json!({"ok": false, "error": error})),
        },
        "/api/short/rename" => match library::rename_short(&app.job.base_dir(), body["id"].as_str().unwrap_or(""), body["titulo"].as_str().unwrap_or("")) {
            Ok(()) => (200, json!({"ok": true})),
            Err(error) => (400, json!({"ok": false, "error": error})),
        },
        "/api/profiles/save" => match library::save_profile(&app.root, body["name"].as_str().unwrap_or(""), &body["data"]) {
            Ok(()) => (200, json!({"ok": true, "profiles": library::load_profiles(&app.root)})),
            Err(error) => (400, json!({"ok": false, "error": error})),
        },
        "/api/profiles/delete" => match library::delete_profile(&app.root, body["name"].as_str().unwrap_or("")) {
            Ok(()) => (200, json!({"ok": true, "profiles": library::load_profiles(&app.root)})),
            Err(error) => (400, json!({"ok": false, "error": error})),
        },
        "/api/queue/clear" => {
            app.job.clear_queue();
            (200, json!({"ok": true}))
        }
        "/api/plan" => (200, plan_state(app)),
        "/api/plan/save" => save_plan(app, &body),
        "/api/plan/render" => render_plan(app, &body),
        "/api/title" => (200, probe_title(body["url"].as_str().unwrap_or(""), &app.titles)),
        "/api/update/check" => {
            app.start_update_check();
            (200, json!({"ok": true}))
        }
        "/api/update/install" => match app.start_update_install() {
            Ok(()) => (200, json!({"ok": true})),
            Err(error) => (400, json!({"ok": false, "error": error})),
        },
        "/api/setup/start" => {
            let tools = if body["gpu"].as_bool().unwrap_or(false) {
                Ok(vec![setup::Tool::WhisperCuda])
            } else if let Some(size) = body["model"].as_str() {
                match setup::MODELS.iter().find(|(name, _)| *name == size) {
                    Some((name, _)) => Ok(vec![setup::Tool::Model(name)]),
                    None => Err(format!("Modelo desconocido: {size}")),
                }
            } else {
                Ok(setup::missing(&app.root))
            };
            match tools {
                Ok(tools) => {
                    app.start_setup(tools);
                    (200, json!({"ok": true}))
                }
                Err(error) => (400, json!({"ok": false, "error": error})),
            }
        }
        "/api/browse" => browse(app, body["kind"].as_str().unwrap_or("file")),
        _ => return respond_error(stream, 404, "no encontrado"),
    };
    respond_json(stream, status, &answer);
}

fn run_form(app: &Arc<App>, mut form: Value) -> (u16, Value) {
    let missing: Vec<&str> = ["podcast", "background"].into_iter().filter(|k| !truthy(&form[*k])).collect();
    if !missing.is_empty() {
        return (400, json!({"ok": false, "error": format!("Falta rellenar: {}", missing.join(", "))}));
    }
    if truthy(&form["review"]) {
        // Revisar antes de montar: se guarda un plan y no se renderiza nada.
        form["plan_out"] = json!(new_plan_path(&app.root).to_string_lossy());
        if form.get("propose").is_none() {
            form["propose"] = json!(0);
        }
    }
    (200, app.job.enqueue(form))
}

fn plan_state(app: &Arc<App>) -> Value {
    match app.job.plan_path().filter(|p| p.is_file()) {
        Some(path) => json!({"ok": true, "plan": {"ruta": path.to_string_lossy(), "clips": plan_clips(&path)}}),
        None => json!({"ok": true, "plan": null}),
    }
}

/// Los candidatos de un plan, tal y como los pinta la interfaz.
fn plan_clips(path: &Path) -> Vec<Value> {
    plan::load(path).map(|p| pipeline::plan_events(&p)).unwrap_or_default()
}

fn plan_target(app: &Arc<App>, body: &Value) -> Option<PathBuf> {
    let path = body["ruta"].as_str().filter(|r| !r.is_empty()).map(PathBuf::from).or_else(|| app.job.plan_path())?;
    path.is_file().then_some(path)
}

fn save_plan(app: &Arc<App>, body: &Value) -> (u16, Value) {
    let Some(path) = plan_target(app, body) else {
        return (400, json!({"ok": false, "error": "no hay ningun plan guardado"}));
    };
    if let Some(changes) = body["clips"].as_array().filter(|c| !c.is_empty()) {
        if let Err(error) = plan::update(&path, changes) {
            return (500, json!({"ok": false, "error": error.to_string()}));
        }
    }
    let overrides: Map<String, Value> = style_pairs(&json!({"style": body["style"]})).into_iter().collect();
    if !overrides.is_empty() {
        if let Err(error) = plan::apply_config(&path, &overrides) {
            return (500, json!({"ok": false, "error": error.to_string()}));
        }
    }
    let clips = plan_clips(&path);
    app.job.set_plan(path, clips.clone());
    (200, json!({"ok": true, "clips": clips}))
}

fn render_plan(app: &Arc<App>, body: &Value) -> (u16, Value) {
    let Some(path) = plan_target(app, body) else {
        return (400, json!({"ok": false, "error": "no hay ningun plan guardado"}));
    };
    let only: Vec<i64> = body["only"].as_array().map(|o| o.iter().filter_map(|n| n.as_f64()).map(|n| n as i64).collect()).unwrap_or_default();
    if only.is_empty() {
        return (400, json!({"ok": false, "error": "no has marcado ningun momento"}));
    }
    let podcast = body["podcast"].as_str().filter(|p| !p.is_empty()).map(str::to_string).unwrap_or_else(|| path.file_name().unwrap_or_default().to_string_lossy().into_owned());
    (
        200,
        app.job.enqueue(json!({
            "from_plan": path.to_string_lossy(),
            "only": only,
            "output": body["output"].as_str().unwrap_or(""),
            "podcast": podcast,
            "clips": only.len(),
        })),
    )
}

fn browse(app: &Arc<App>, kind: &str) -> (u16, Value) {
    match &app.picker {
        Some(pick) => (200, json!({"ok": true, "path": pick(kind).unwrap_or_default()})),
        None => (501, json!({"ok": false, "error": "el dialogo de ficheros no esta disponible; pega la ruta a mano."})),
    }
}

// ----------------------------------------------------------------------------
// Arranque
// ----------------------------------------------------------------------------

fn free_listener(preferred: u16) -> std::io::Result<TcpListener> {
    for port in preferred..preferred.saturating_add(20) {
        if let Ok(listener) = TcpListener::bind(("127.0.0.1", port)) {
            return Ok(listener);
        }
    }
    Err(std::io::Error::other(format!("No hay ningun puerto libre entre {preferred} y {}.", preferred.saturating_add(19))))
}

/// Atiende conexiones para siempre, una por hilo. El servidor solo escucha en
/// 127.0.0.1: esto abre procesos en la maquina, no debe salir a la red.
pub fn serve_forever(app: Arc<App>, listener: TcpListener) {
    for stream in listener.incoming().flatten() {
        let app = app.clone();
        std::thread::spawn(move || handle_connection(&app, stream));
    }
}

pub fn open_browser(url: &str) {
    let result = if cfg!(windows) {
        ffmpeg::command("cmd").args(["/C", "start", "", url]).spawn()
    } else if cfg!(target_os = "macos") {
        Command::new("open").arg(url).spawn()
    } else {
        Command::new("xdg-open").arg(url).spawn()
    };
    if let Err(error) = result {
        log::warn!("No se pudo abrir el navegador ({error}).");
    }
}

/// El servidor ya en marcha (en su propio hilo): la aplicacion, el puerto y la
/// direccion completa, con el token.
pub struct Running {
    pub app: Arc<App>,
    pub port: u16,
    pub url: String,
}

/// Prepara la carpeta de datos y levanta el servidor local en un hilo.
pub fn launch(port: u16) -> Result<Running, String> {
    let root = ffmpeg::project_root();
    ffmpeg::ensure_home(&root, include_str!("../../config.yaml"));
    // Todo lo relativo (config.yaml, work/, output/) se resuelve desde la carpeta
    // de datos, se lance desde donde se lance.
    let _ = std::env::set_current_dir(&root);

    let listener = free_listener(port).map_err(|e| e.to_string())?;
    let port = listener.local_addr().map_or(port, |a| a.port());
    let app = App::new(&root, real_spawner());
    let url = format!("http://127.0.0.1:{port}/?t={}", app.token);

    let _ = progress::mode(); // se resuelve aqui, no a mitad de un trabajo
    let serving = app.clone();
    std::thread::spawn(move || serve_forever(serving, listener));
    // Sin prisa: que el arranque y la primera pantalla no esperen a la red.
    let checking = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(3));
        checking.start_update_check();
    });
    Ok(Running { app, port, url })
}

/// `viralshorts gui`: la interfaz en el navegador, para quien lo prefiera o no
/// tenga WebView2. No vuelve hasta Ctrl+C.
pub fn run(port: u16, browser: bool) -> i32 {
    let running = match launch(port) {
        Ok(running) => running,
        Err(error) => {
            eprintln!("{error}");
            return 2;
        }
    };
    {
        let job = running.app.job.clone();
        let _ = ctrlc::set_handler(move || {
            job.stop();
            std::process::exit(130);
        });
    }

    println!("{}", "=".repeat(62));
    println!(" viralshorts - interfaz visual");
    println!("{}", "=".repeat(62));
    println!(" Abre esta direccion si no se abre sola:\n   {}", running.url);
    println!(" Ctrl+C para apagar la interfaz.");
    println!("{}", "=".repeat(62));
    let _ = std::io::stdout().flush();
    if browser {
        open_browser(&running.url);
    }

    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}

/// Cuanto tarda en pasar a "terminado" un trabajo (para las pruebas).
pub fn wait_for_status(job: &Job, wanted: &str, timeout: Duration) -> bool {
    let started = Instant::now();
    while started.elapsed() < timeout {
        if job.status() == wanted {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- una interfaz con procesos de mentira ------------------------------------------------

    /// Proceso de mentira: suelta unas lineas y espera a que alguien lo libere o lo pare.
    struct Falso {
        lineas: Vec<String>,
        codigo: i32,
        liberar: Receiver<()>,
        parado: Arc<Mutex<bool>>,
        soltar: Sender<()>,
    }

    impl Process for Falso {
        fn read_lines(&mut self, on_line: &mut dyn FnMut(String)) {
            for linea in self.lineas.drain(..) {
                on_line(linea);
            }
            let _ = self.liberar.recv_timeout(Duration::from_secs(5));
        }

        fn wait(&mut self) -> i32 {
            self.codigo
        }

        fn killer(&self) -> Box<dyn Fn() + Send + Sync> {
            let parado = self.parado.clone();
            let soltar = self.soltar.clone();
            Box::new(move || {
                *parado.lock().unwrap() = true;
                let _ = soltar.send(());
            })
        }
    }

    struct Banco {
        job: Arc<Job>,
        lanzados: Arc<Mutex<Vec<Vec<String>>>>,
        liberadores: Arc<Mutex<Vec<Sender<()>>>>,
        parado: Arc<Mutex<bool>>,
    }

    fn banco(lineas: Vec<&str>, codigo: i32, fallar: bool) -> Banco {
        let lanzados: Arc<Mutex<Vec<Vec<String>>>> = Arc::default();
        let liberadores: Arc<Mutex<Vec<Sender<()>>>> = Arc::default();
        let parado = Arc::new(Mutex::new(false));
        let (l, lib, par) = (lanzados.clone(), liberadores.clone(), parado.clone());
        let lineas: Vec<String> = lineas.into_iter().map(String::from).collect();
        let spawner: Spawner = Arc::new(move |args, _cwd| {
            if fallar {
                return Err(std::io::Error::other("sin ejecutable"));
            }
            l.lock().unwrap().push(args.to_vec());
            let (soltar, liberar) = mpsc::channel();
            lib.lock().unwrap().push(soltar.clone());
            Ok(Box::new(Falso { lineas: lineas.clone(), codigo, liberar, parado: par.clone(), soltar }) as Box<dyn Process>)
        });
        let dir = tempfile::tempdir().unwrap().keep();
        let job = Job::with_handoff(&dir, spawner, Duration::from_millis(5));
        Banco { job, lanzados, liberadores, parado }
    }

    fn formulario(n: usize) -> Value {
        json!({"podcast": format!("p{n}"), "background": "b"})
    }

    // -- seguridad ----------------------------------------------------------------------------

    /// Una peticion a mano, con el Host que se quiera.
    fn pedir(port: u16, metodo: &str, ruta: &str, host: &str, token: Option<&str>, cuerpo: &str, extra: &[(&str, &str)]) -> (u16, String, String) {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let mut texto = format!("{metodo} {ruta} HTTP/1.1\r\nHost: {host}\r\nContent-Length: {}\r\nConnection: close\r\n", cuerpo.len());
        if let Some(t) = token {
            texto.push_str(&format!("X-VS-Token: {t}\r\n"));
        }
        for (k, v) in extra {
            texto.push_str(&format!("{k}: {v}\r\n"));
        }
        texto.push_str("\r\n");
        stream.write_all(texto.as_bytes()).unwrap();
        stream.write_all(cuerpo.as_bytes()).unwrap();
        let mut bruto = Vec::new();
        stream.read_to_end(&mut bruto).unwrap();
        let bruto = String::from_utf8_lossy(&bruto).into_owned();
        let (cabecera, cuerpo) = bruto.split_once("\r\n\r\n").unwrap_or((&bruto, ""));
        let codigo = cabecera.split_whitespace().nth(1).unwrap().parse().unwrap();
        (codigo, cabecera.to_string(), cuerpo.to_string())
    }

    fn servidor(app: &Arc<App>) -> u16 {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let puerto = listener.local_addr().unwrap().port();
        let app = app.clone();
        std::thread::spawn(move || serve_forever(app, listener));
        puerto
    }

    fn app_de_prueba() -> (Arc<App>, u16, Banco) {
        let b = banco(vec![], 0, false);
        let app = App::with_job(&std::env::temp_dir(), b.job.clone(), "token-de-prueba");
        let puerto = servidor(&app);
        (app, puerto, b)
    }

    #[test]
    fn la_pagina_sin_token_no_se_sirve() {
        let (_app, puerto, _b) = app_de_prueba();
        let (codigo, _, cuerpo) = pedir(puerto, "GET", "/", "127.0.0.1:8760", None, "", &[]);
        assert_eq!(codigo, 403);
        assert!(!cuerpo.contains("token-de-prueba"));
    }

    #[test]
    fn la_pagina_con_token_en_la_url_se_sirve_con_el_token_dentro() {
        let (_app, puerto, _b) = app_de_prueba();
        let (codigo, _, cuerpo) = pedir(puerto, "GET", "/?t=token-de-prueba", "127.0.0.1:8760", None, "", &[]);
        assert_eq!(codigo, 200);
        assert!(cuerpo.contains("token-de-prueba") && !cuerpo.contains("__TOKEN__"));
    }

    #[test]
    fn dns_rebinding_un_dominio_ajeno_no_obtiene_ni_la_pagina() {
        let (_app, puerto, _b) = app_de_prueba();
        let (codigo, _, cuerpo) = pedir(puerto, "GET", "/?t=token-de-prueba", "evil.example:8760", None, "", &[]);
        assert_eq!(codigo, 403);
        assert!(!cuerpo.contains("token-de-prueba"));
    }

    #[test]
    fn los_datos_tambien_piden_token() {
        let (_app, puerto, _b) = app_de_prueba();
        for ruta in ["/api/state", "/api/existing", "/api/video?f=short_1.mp4"] {
            assert_eq!(pedir(puerto, "GET", ruta, "127.0.0.1", None, "", &[]).0, 403, "{ruta}");
        }
        assert_eq!(pedir(puerto, "GET", "/api/existing", "127.0.0.1", Some("token-de-prueba"), "", &[]).0, 200);
        assert_eq!(pedir(puerto, "GET", "/api/existing", "127.0.0.1", Some("token-malo"), "", &[]).0, 403);
        assert_eq!(pedir(puerto, "GET", "/api/existing?t=%C3%B1%C3%B1", "127.0.0.1", None, "", &[]).0, 403, "un token con caracteres raros no rompe el servidor");
    }

    #[test]
    fn post_con_token_pero_host_ajeno_o_sin_token() {
        let (_app, puerto, _b) = app_de_prueba();
        assert_eq!(pedir(puerto, "POST", "/api/stop", "evil.example", Some("token-de-prueba"), "{}", &[]).0, 403);
        assert_eq!(pedir(puerto, "POST", "/api/stop", "127.0.0.1", None, "{}", &[]).0, 403);
        assert_eq!(pedir(puerto, "POST", "/api/stop", "localhost:8760", Some("token-de-prueba"), "{}", &[]).0, 200);
    }

    #[test]
    fn hosts_permitidos() {
        for host in ["127.0.0.1:8760", "localhost:8760", "LOCALHOST", "[::1]:8760"] {
            assert!(host_allowed(host), "{host}");
        }
        for host in ["", "evil.example", "127.0.0.1.evil.example:8760", "localhost.evil.example"] {
            assert!(!host_allowed(host), "{host}");
        }
    }

    #[test]
    fn el_token_es_url_safe_y_distinto_cada_vez() {
        let (a, b) = (new_token(), new_token());
        assert_eq!(a.len(), 22);
        assert_ne!(a, b);
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }

    #[test]
    fn rutas_post_desconocidas_y_cuerpos_rotos() {
        let (_app, puerto, _b) = app_de_prueba();
        assert_eq!(pedir(puerto, "POST", "/api/nada", "127.0.0.1", Some("token-de-prueba"), "{}", &[]).0, 404);
        assert_eq!(pedir(puerto, "POST", "/api/run", "127.0.0.1", Some("token-de-prueba"), "{roto", &[]).0, 500);
        let (codigo, _, cuerpo) = pedir(puerto, "POST", "/api/run", "127.0.0.1", Some("token-de-prueba"), r#"{"podcast": "x"}"#, &[]);
        assert_eq!(codigo, 400);
        assert!(cuerpo.contains("background"), "{cuerpo}");
        assert_eq!(pedir(puerto, "POST", "/api/browse", "127.0.0.1", Some("token-de-prueba"), "{}", &[]).0, 501, "sin dialogo, a mano");
    }

    // -- videos -----------------------------------------------------------------------------------

    fn con_videos() -> (Arc<App>, u16, Banco, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("short_1.mp4"), b"0123456789").unwrap();
        std::fs::write(dir.path().parent().unwrap().join("secreto.mp4"), b"no").unwrap();
        let (app, puerto, b) = app_de_prueba();
        app.job.set_output_dir(dir.path().to_path_buf());
        (app, puerto, b, dir)
    }

    fn video(puerto: u16, nombre: &str, extra: &[(&str, &str)]) -> (u16, String, String) {
        pedir(puerto, "GET", &format!("/api/video?f={nombre}&t=token-de-prueba"), "127.0.0.1", None, "", extra)
    }

    #[test]
    fn sirve_el_video_y_los_rangos() {
        let (_app, puerto, _b, _dir) = con_videos();
        let (codigo, _, cuerpo) = video(puerto, "short_1.mp4", &[]);
        assert_eq!((codigo, cuerpo.as_str()), (200, "0123456789"));

        let (codigo, cabecera, cuerpo) = video(puerto, "short_1.mp4", &[("Range", "bytes=2-5")]);
        assert_eq!((codigo, cuerpo.as_str()), (206, "2345"));
        assert!(cabecera.contains("Content-Range: bytes 2-5/10"), "{cabecera}");

        assert_eq!(video(puerto, "short_1.mp4", &[("Range", "bytes=7-")]).2, "789");
        assert_eq!(video(puerto, "short_1.mp4", &[("Range", "bytes=-3")]).2, "789", "los ultimos tres bytes");
        assert_eq!(video(puerto, "short_1.mp4", &[("Range", "bytes=99-")]).0, 416);
    }

    #[test]
    fn no_se_sale_de_la_carpeta() {
        let (_app, puerto, _b, _dir) = con_videos();
        for nombre in ["../secreto.mp4", "..%5Csecreto.mp4", "%2e%2e/secreto.mp4", "C:%5Cwindows%5Cx.mp4", "short_1.txt", ".mp4", "a%00.mp4"] {
            let (codigo, _, cuerpo) = video(puerto, nombre, &[]);
            assert_eq!(codigo, 400, "{nombre}");
            assert_ne!(cuerpo, "no");
        }
        assert_eq!(video(puerto, "short_9.mp4", &[]).0, 404);
    }

    #[test]
    fn nombres_de_medios_validos() {
        for ok in ["short_1.mp4", "short 2.jpg", "a-b.c.webm"] {
            assert!(valid_media_name(ok), "{ok}");
        }
        for mal in ["", ".mp4", "x.exe", "a/b.mp4", "a\\b.mp4", "..", "sin_extension"] {
            assert!(!valid_media_name(mal), "{mal}");
        }
    }

    #[test]
    fn eventos_en_directo_empiezan_con_el_estado() {
        let (_app, puerto, _b) = app_de_prueba();
        let mut stream = TcpStream::connect(("127.0.0.1", puerto)).unwrap();
        stream.write_all(b"GET /api/events?t=token-de-prueba HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        // La cabecera y el primer evento pueden llegar en lecturas distintas.
        let mut texto = String::new();
        let mut bytes = vec![0u8; 8192];
        while !texto.contains("data: {") || !texto.ends_with("\n\n") {
            let leidos = stream.read(&mut bytes).unwrap();
            assert!(leidos > 0, "el servidor cerro la conexion: {texto}");
            texto.push_str(&String::from_utf8_lossy(&bytes[..leidos]));
        }
        assert!(texto.contains("text/event-stream") && texto.contains("data: {"), "{texto}");
        assert!(texto.contains("\"type\":\"hello\"") && texto.contains("\"status\":\"idle\""), "{texto}");
    }

    // -- cola -------------------------------------------------------------------------------------

    #[test]
    fn ejecuciones_simultaneas_arrancan_solo_una() {
        let b = banco(vec![], 0, false);
        let barrera = Arc::new(std::sync::Barrier::new(16));
        let hilos: Vec<_> = (0..16)
            .map(|n| {
                let (job, barrera) = (b.job.clone(), barrera.clone());
                std::thread::spawn(move || {
                    barrera.wait();
                    job.enqueue(formulario(n));
                })
            })
            .collect();
        for hilo in hilos {
            hilo.join().unwrap();
        }
        assert_eq!(b.lanzados.lock().unwrap().len(), 1);
        assert_eq!(b.job.pending_len(), 15);
        b.job.stop();
    }

    #[test]
    fn al_terminar_arranca_el_siguiente_de_la_cola() {
        let b = banco(vec![], 0, false);
        b.job.enqueue(formulario(1));
        let respuesta = b.job.enqueue(formulario(2));
        assert_eq!(respuesta["encolado"], true);
        assert_eq!(respuesta["posicion"], 1);

        b.liberadores.lock().unwrap()[0].send(()).unwrap();
        let t = Instant::now();
        while b.lanzados.lock().unwrap().len() < 2 && t.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(5));
        }
        let lanzados = b.lanzados.lock().unwrap().clone();
        assert_eq!(lanzados.len(), 2);
        assert!(lanzados[1].contains(&"p2".to_string()));
        assert_eq!(b.job.pending_len(), 0);
        b.job.stop();
    }

    #[test]
    fn la_cola_se_reordena_y_se_recorta() {
        let b = banco(vec![], 0, false);
        b.job.enqueue(formulario(1));
        for n in 2..=4 {
            b.job.enqueue(formulario(n));
        }
        let orden = |job: &Arc<Job>| -> Vec<String> {
            job.snapshot()["queue"].as_array().unwrap().iter().map(|q| q["podcast"].as_str().unwrap().to_string()).collect()
        };
        assert_eq!(orden(&b.job), ["p2", "p3", "p4"]);
        assert!(b.job.move_pending(3, -1));
        assert_eq!(orden(&b.job), ["p2", "p4", "p3"]);
        assert!(!b.job.move_pending(1, -1), "el primero no puede adelantarse");
        assert!(!b.job.move_pending(3, 1), "el ultimo no puede atrasarse");
        assert!(b.job.move_pending(1, 5), "solo cuenta el sentido");
        assert_eq!(orden(&b.job), ["p4", "p2", "p3"]);
        assert!(b.job.remove_pending(2));
        assert_eq!(orden(&b.job), ["p4", "p3"]);
        assert!(!b.job.remove_pending(0) && !b.job.remove_pending(9));
        b.job.stop();
    }

    #[test]
    fn saltar_el_actual_deja_seguir_a_la_cola() {
        let b = banco(vec![], 0, false);
        b.job.enqueue(formulario(1));
        b.job.enqueue(formulario(2));
        b.job.stop_current();
        let t = Instant::now();
        while b.lanzados.lock().unwrap().len() < 2 && t.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(b.lanzados.lock().unwrap().len(), 2, "el segundo arranca solo");
        assert_eq!(b.job.pending_len(), 0);
        b.job.stop();
    }

    #[test]
    fn las_cookies_solo_admiten_navegadores_conocidos() {
        let con = |cookies: &str| style_pairs(&json!({"style": {"cookies": cookies}}));
        assert_eq!(con("Firefox"), vec![("download.cookies_from_browser".to_string(), json!("firefox"))]);
        assert!(con("").is_empty() && con("chrome --evil").is_empty() && con("../x").is_empty());
    }

    #[test]
    fn los_ajustes_de_transcripcion_pasan_filtrados() {
        let con = |style: Value| -> std::collections::HashMap<String, Value> { style_pairs(&json!({"style": style})).into_iter().collect() };
        let ok = con(json!({"refine": "large-v3-turbo", "vocab": "Ibai, Auron", "corrections": "spotifai=Spotify
la guapa=La Guapa"}));
        assert_eq!(ok["transcribe.refine_model"], "large-v3-turbo");
        assert_eq!(ok["transcribe.initial_prompt"], "Ibai, Auron");
        assert_eq!(ok["transcribe.corrections"], "spotifai=Spotify;la guapa=La Guapa", "los saltos de linea pasan a ;");
        // Un modelo inventado no llega al pipeline.
        assert!(!con(json!({"refine": "gigante"})).contains_key("transcribe.refine_model"));
        assert!(con(json!({"refine": ""})).is_empty(), "vacio = no se manda y vale lo de config.yaml");
        let largo = con(json!({"vocab": "x".repeat(2000)}));
        assert_eq!(largo["transcribe.initial_prompt"].as_str().unwrap().len(), 600);
    }

    #[test]
    fn biblioteca_borrar_renombrar_y_perfiles_por_http() {
        let dir = tempfile::tempdir().unwrap();
        let (app, puerto, _b) = app_de_prueba();
        app.job.set_base_dir(dir.path().to_path_buf());
        let run = dir.path().join("pod_2026");
        std::fs::create_dir_all(&run).unwrap();
        std::fs::write(run.join("short_1.mp4"), b"v").unwrap();
        std::fs::write(run.join("short_1.json"), json!({"publicacion": {"title": "a"}}).to_string()).unwrap();
        let t = Some("token-de-prueba");
        let post = |ruta: &str, cuerpo: &str| pedir(puerto, "POST", ruta, "127.0.0.1", t, cuerpo, &[]);

        let (codigo, _, cuerpo) = pedir(puerto, "GET", "/api/existing", "127.0.0.1", t, "", &[]);
        assert_eq!(codigo, 200);
        assert!(cuerpo.contains("pod_2026/short_1.mp4"), "{cuerpo}");
        // El video de una subcarpeta se sirve por su id.
        assert_eq!(pedir(puerto, "GET", "/api/video?f=pod_2026/short_1.mp4", "127.0.0.1", t, "", &[]).0, 200);

        assert_eq!(post("/api/short/rename", r#"{"id": "pod_2026/short_1.mp4", "titulo": "Nuevo"}"#).0, 200);
        assert!(pedir(puerto, "GET", "/api/existing", "127.0.0.1", t, "", &[]).2.contains("Nuevo"));
        assert_eq!(post("/api/short/rename", r#"{"id": "../x/short_1.mp4", "titulo": "Nuevo"}"#).0, 400);

        assert_eq!(post("/api/short/delete", r#"{"id": "pod_2026/short_1.mp4"}"#).0, 200);
        assert!(!run.join("short_1.mp4").exists() && !run.join("short_1.json").exists());
        assert_eq!(post("/api/short/delete", r#"{"id": "pod_2026/short_1.mp4"}"#).0, 400);

        assert_eq!(post("/api/profiles/save", r#"{"name": "Humor", "data": {"clips": 5}}"#).0, 200);
        let (_, _, perfiles) = pedir(puerto, "GET", "/api/profiles", "127.0.0.1", t, "", &[]);
        assert!(perfiles.contains("Humor"), "{perfiles}");
        assert_eq!(post("/api/profiles/save", r#"{"name": "", "data": {}}"#).0, 400);
        assert_eq!(post("/api/profiles/delete", r#"{"name": "Humor"}"#).0, 200);
        assert_eq!(post("/api/profiles/delete", r#"{"name": "Humor"}"#).0, 400);

        // Modelos de voz: la lista existe y un nombre inventado se rechaza.
        let (codigo, _, modelos) = pedir(puerto, "GET", "/api/models", "127.0.0.1", t, "", &[]);
        assert_eq!(codigo, 200);
        assert!(modelos.contains("medium"));
        assert_eq!(post("/api/setup/start", r#"{"model": "gigante"}"#).0, 400);
        // Sin token, nada de esto.
        assert_eq!(pedir(puerto, "POST", "/api/short/delete", "127.0.0.1", None, "{}", &[]).0, 403);
        assert_eq!(pedir(puerto, "GET", "/api/frame?src=C:/x.mp4", "127.0.0.1", None, "", &[]).0, 403);
    }

    #[test]
    fn detener_vacia_la_cola_y_mata_el_proceso() {
        let b = banco(vec![], 0, false);
        b.job.enqueue(formulario(1));
        b.job.enqueue(formulario(2));
        b.job.stop();
        assert!(*b.parado.lock().unwrap());
        assert!(wait_for_status(&b.job, "stopped", Duration::from_secs(5)));
        assert_eq!(b.job.pending_len(), 0);
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(b.lanzados.lock().unwrap().len(), 1, "no arranca el siguiente");
        assert_eq!(b.job.status(), "stopped");
    }

    #[test]
    fn si_no_se_puede_lanzar_no_se_queda_en_running() {
        let b = banco(vec![], 0, true);
        b.job.enqueue(formulario(1));
        assert_eq!(b.job.status(), "error");
        let instantanea = b.job.snapshot();
        assert!(instantanea["log"].as_array().unwrap().iter().any(|l| l.as_str().unwrap().contains("No se pudo lanzar")));
    }

    #[test]
    fn un_proceso_que_falla_deja_el_estado_en_error_y_uno_que_acaba_en_done() {
        for (codigo, esperado) in [(0, "done"), (3, "error")] {
            let b = banco(vec![], codigo, false);
            b.job.enqueue(formulario(1));
            b.liberadores.lock().unwrap()[0].send(()).unwrap();
            assert!(wait_for_status(&b.job, esperado, Duration::from_secs(5)), "{esperado}");
        }
    }

    #[test]
    fn los_eventos_del_pipeline_actualizan_el_estado_y_se_reparten() {
        let lineas = vec![
            r#"@@VS {"type":"stage","stage":"transcripcion","label":"Transcribiendo el podcast"}"#,
            r#"@@VS {"type":"clips","clips":[{"n":1}]}"#,
            r#"@@VS {"type":"salida","ruta":"C:/x/podcast_2026"}"#,
            r#"@@VS {"type":"aviso","titulo":"Falta la fuente"}"#,
            r#"@@VS {"type":"clip_done","n":1,"fichero":"short_1.mp4"}"#,
            r#"@@VS {"type":"plan","ruta":"work/plans/p.json","clips":[{"n":1}]}"#,
            "@@VS {roto",
            "una linea de log normal",
            "",
        ];
        let b = banco(lineas, 0, false);
        let eventos = b.job.subscribe();
        b.job.enqueue(formulario(1));
        b.liberadores.lock().unwrap()[0].send(()).unwrap();
        assert!(wait_for_status(&b.job, "done", Duration::from_secs(5)));

        let estado = b.job.snapshot();
        assert_eq!(estado["stage"], "Transcribiendo el podcast");
        assert_eq!(estado["clips"], json!([{"n": 1}]));
        assert_eq!(estado["results"][0]["fichero"], "short_1.mp4");
        assert_eq!(estado["notices"][0]["titulo"], "Falta la fuente");
        assert_eq!(estado["output_dir"].as_str().unwrap().replace('\\', "/"), "C:/x/podcast_2026");
        assert_eq!(estado["plan"]["ruta"], "work/plans/p.json");
        assert!(estado["log"].as_array().unwrap().iter().any(|l| l == "una linea de log normal"));

        let tipos: Vec<String> = eventos.try_iter().map(|e| e["type"].as_str().unwrap_or("").to_string()).collect();
        for esperado in ["job", "stage", "clips", "salida", "aviso", "clip_done", "plan", "log"] {
            assert!(tipos.contains(&esperado.to_string()), "falta {esperado}: {tipos:?}");
        }
    }

    #[test]
    fn un_formulario_de_plan_lanza_from_plan() {
        let b = banco(vec![], 0, false);
        b.job.enqueue(json!({"from_plan": "work/plans/p.json", "only": [1, 4], "output": "salida", "podcast": "p"}));
        let lanzado = b.lanzados.lock().unwrap()[0].clone();
        assert_eq!(lanzado, ["--from-plan", "work/plans/p.json", "--only", "1,4", "--output", "salida"]);
        b.job.stop();
    }

    #[test]
    fn un_cliente_desconectado_se_olvida_sin_romper_nada() {
        let b = banco(vec![], 0, false);
        drop(b.job.subscribe());
        b.job.publish(json!({"type": "log"}));
        assert_eq!(b.job.subscribers.lock().unwrap().len(), 0);
    }

    // -- carpeta de salida -------------------------------------------------------------------------

    #[test]
    fn elige_la_ejecucion_mas_reciente_con_shorts() {
        let dir = tempfile::tempdir().unwrap();
        let (vieja, nueva, vacia) = (dir.path().join("a_1"), dir.path().join("b_2"), dir.path().join("c_3"));
        for carpeta in [&vieja, &nueva, &vacia] {
            std::fs::create_dir(carpeta).unwrap();
        }
        std::fs::write(vieja.join("short_1.mp4"), b"x").unwrap();
        std::thread::sleep(Duration::from_millis(50));
        std::fs::write(nueva.join("short_1.mp4"), b"x").unwrap();
        // El tiempo de modificacion de la carpeta cambia al escribir dentro.
        assert_eq!(latest_run_dir(dir.path()), nueva);
    }

    #[test]
    fn sin_subcarpetas_se_queda_con_la_base() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(latest_run_dir(dir.path()), dir.path());
        std::fs::write(dir.path().join("short_1.mp4"), b"x").unwrap();
        assert_eq!(latest_run_dir(dir.path()), dir.path());
        assert_eq!(latest_run_dir(Path::new("no/existe")), Path::new("no/existe"));
    }

    #[test]
    fn los_shorts_ya_generados_con_sus_textos() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("short_1.mp4"), b"x").unwrap();
        std::fs::write(dir.path().join("short_2.mp4"), b"x").unwrap();
        std::fs::write(
            dir.path().join("short_1.json"),
            json!({"fuente": {"marca": "9:23"}, "publicacion": {"title": "T", "hook": "G", "description": "Desc.", "hashtags": ["a", "b"]}}).to_string(),
        )
        .unwrap();
        let hallados = scan_output(dir.path());
        assert_eq!(hallados.len(), 2);
        assert_eq!((hallados[0]["titulo"].clone(), hallados[0]["pie"].clone(), hallados[0]["marca"].clone()), (json!("T"), json!("Desc.\n\n#a #b"), json!("9:23")));
        assert_eq!(hallados[1]["titulo"], "", "un short sin json sale igualmente");
        assert!(scan_output(Path::new("no/existe")).is_empty());
    }

    // -- linea de comandos -----------------------------------------------------------------------------

    fn comando(extra: Value) -> Vec<String> {
        let mut form = json!({"podcast": "https://youtu.be/abc", "background": "juego.mp4"});
        form.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        build_command(&form)
    }

    fn pares(args: &[String]) -> Vec<String> {
        args.iter().enumerate().filter(|(_, a)| *a == "--set").map(|(i, _)| args[i + 1].clone()).collect()
    }

    fn tras<'a>(args: &'a [String], bandera: &str) -> &'a str {
        &args[args.iter().position(|a| a == bandera).unwrap() + 1]
    }

    #[test]
    fn lo_minimo() {
        let args = comando(json!({}));
        assert_eq!(args, ["--podcast", "https://youtu.be/abc", "--background", "juego.mp4"]);
    }

    #[test]
    fn los_interruptores_apagados_pasan_su_bandera() {
        let args = comando(json!({"llm": false, "reframe": false, "censor": false, "prescan": false, "zoom": true}));
        for bandera in ["--no-llm", "--no-reframe", "--no-censor", "--full-transcript"] {
            assert!(args.contains(&bandera.to_string()), "{bandera}");
        }
        assert!(!args.contains(&"--no-zoom".to_string()));
    }

    #[test]
    fn los_numeros_llegan_como_numeros() {
        let args = comando(json!({"clips": "5", "min_duration": "20", "head_skip": 0, "beam": 1, "regions": "9.0"}));
        assert_eq!(tras(&args, "--clips"), "5");
        assert_eq!(tras(&args, "--min-duration"), "20.0");
        assert_eq!(tras(&args, "--head-skip"), "0.0");
        assert_eq!(tras(&args, "--beam"), "1");
        assert_eq!(tras(&args, "--regions"), "9");
    }

    #[test]
    fn estilo_valido() {
        let args = comando(json!({"style": {"font_size": "80", "color_active": "#ffcc00", "position": 1.7, "words": 3, "encoder": " off "}}));
        let p = pares(&args);
        for esperado in ["subtitles.font_size=80", "subtitles.color_active=#FFCC00", "subtitles.vertical_position_ratio=1.0", "subtitles.max_words_per_group=3", "output.hardware_encoder=off"] {
            assert!(p.contains(&esperado.to_string()), "{esperado} en {p:?}");
        }
    }

    #[test]
    fn estilo_invalido_se_ignora_y_el_navegador_no_elige_el_ajuste() {
        let args = comando(json!({"style": {"color_active": "rojo", "font_size": "mucho", "ajuste_inventado": "x", "font_name": "", "llm.model": "otro", "output.dir": "/"}}));
        assert!(pares(&args).is_empty(), "{:?}", pares(&args));
    }

    #[test]
    fn los_ajustes_de_estilo_valen_para_el_programa() {
        // Todo lo que la interfaz puede mandar tiene que entenderlo `--set`.
        let mut cfg = Config::default();
        let estilo = json!({"style": {"font_name": "Impact", "font_size": 70, "color_active": "#00FF00", "position": 0.25, "words": 2, "hook_size": 50, "max_height": 720, "workers": 2, "encoder": "off"}});
        for (ajuste, valor) in style_pairs(&estilo) {
            cfg.set(&format!("{ajuste}={}", value_text(&valor))).unwrap_or_else(|e| panic!("{ajuste}: {e}"));
        }
        assert_eq!((cfg.subtitles.font_size, cfg.subtitles.font_name.as_str(), cfg.download.max_height, cfg.output.render_workers), (70, "Impact", 720, 2));
        assert_eq!(cfg.subtitles.vertical_position_ratio, 0.25);
    }

    #[test]
    fn plan_en_dos_tiempos() {
        let args = comando(json!({"plan_out": "work/plans/p.json", "propose": 9, "dry_run": true}));
        assert_eq!(tras(&args, "--plan-out"), "work/plans/p.json");
        assert_eq!(tras(&args, "--propose"), "9");
        assert!(!args.contains(&"--dry-run".to_string()));
        assert!(comando(json!({"dry_run": true})).contains(&"--dry-run".to_string()));
    }

    #[test]
    fn los_argumentos_que_manda_la_interfaz_los_entiende_el_pipeline() {
        use clap::Parser;
        #[derive(Parser)]
        struct Cli {
            #[command(flatten)]
            args: pipeline::Args,
        }
        let form = json!({"podcast": "p", "background": "b", "clips": 3, "min_duration": 25, "max_duration": 45, "head_skip": 60, "tail_skip": 30,
            "output": "salida", "model": "small", "language": "es", "beam": 1, "regions": 12, "llm": false, "reframe": false, "zoom": false,
            "jumpcuts": false, "prescan": false, "title_card": false, "context": false, "censor": false, "plan_out": "p.json", "propose": 9,
            "style": {"font_size": 80, "color_active": "#FFF200", "position": 0.3, "words": 3, "hook_size": 60, "max_height": 720, "workers": 0, "encoder": "auto"}});
        let args = build_command(&form);
        let analizado = Cli::try_parse_from(std::iter::once("viralshorts".to_string()).chain(args)).unwrap().args;
        assert_eq!((analizado.clips, analizado.regions, analizado.propose), (Some(3), Some(12), Some(9)));
        assert!(analizado.no_llm && analizado.no_censor && analizado.full_transcript && analizado.plan_out.is_some());
        let mut cfg = Config::default();
        pipeline::apply_overrides(&mut cfg, &analizado).unwrap();
        assert_eq!(cfg.subtitles.font_size, 80);
    }

    // -- ficheros y plan -----------------------------------------------------------------------------------

    #[test]
    fn el_titulo_de_un_fichero_local_sale_del_nombre_y_se_recuerda() {
        let dir = tempfile::tempdir().unwrap();
        let video = dir.path().join("Mi Podcast.mp4");
        std::fs::write(&video, b"x").unwrap();
        let cache = Mutex::new(HashMap::new());
        let respuesta = probe_title(video.to_str().unwrap(), &cache);
        assert_eq!((respuesta["ok"].clone(), respuesta["title"].clone(), respuesta["source"].clone()), (json!(true), json!("Mi Podcast"), json!("archivo")));
        assert_eq!(cache.lock().unwrap().len(), 1);
        assert_eq!(probe_title("   ", &cache)["ok"], false);
    }

    #[test]
    fn guardar_y_montar_un_plan_desde_la_interfaz() {
        let b = banco(vec![], 0, false);
        let dir = tempfile::tempdir().unwrap();
        let app = App::with_job(dir.path(), b.job.clone(), "t");
        let ruta = dir.path().join("plan.json");
        let fuente = plan::PlanSource { ruta: "p.mp4".into(), titulo: "P".into(), duracion: 10.0, url: String::new() };
        let clips: Vec<crate::scoring::Window> = (0..2).map(|i| crate::scoring::Window::new(i as f64 * 100.0, i as f64 * 100.0 + 30.0, vec![], "texto")).collect();
        plan::save(&ruta, &Config::default(), &fuente, &fuente, &clips, &HashMap::new(), 2).unwrap();

        let (codigo, respuesta) = save_plan(&app, &json!({"ruta": ruta.to_string_lossy(), "clips": [{"n": 1, "seleccionado": false}], "style": {"font_size": 99}}));
        assert_eq!(codigo, 200);
        assert_eq!(respuesta["clips"][0]["seleccionado"], false);
        assert_eq!(plan::load(&ruta).unwrap().config["subtitles"]["font_size"], 99);
        assert_eq!(b.job.plan_path(), Some(ruta.clone()));

        assert_eq!(render_plan(&app, &json!({"ruta": ruta.to_string_lossy(), "only": []})).0, 400);
        let (codigo, respuesta) = render_plan(&app, &json!({"ruta": ruta.to_string_lossy(), "only": [2]}));
        assert_eq!((codigo, respuesta["encolado"].clone()), (200, json!(false)));
        assert_eq!(b.lanzados.lock().unwrap()[0][..4], ["--from-plan", ruta.to_string_lossy().as_ref(), "--only", "2"]);
        b.job.stop();

        assert_eq!(save_plan(&app, &json!({"ruta": "no/existe.json"})).0, 400);
        let sin_plan = App::with_job(dir.path(), banco(vec![], 0, false).job, "t");
        assert_eq!(plan_state(&sin_plan)["plan"], Value::Null);
    }

    #[test]
    fn el_estado_de_las_actualizaciones_pide_token_y_cuenta_la_version() {
        let (_app, puerto, _b) = app_de_prueba();
        assert_eq!(pedir(puerto, "GET", "/api/update", "127.0.0.1", None, "", &[]).0, 403);
        let (codigo, _, cuerpo) = pedir(puerto, "GET", "/api/update", "127.0.0.1", Some("token-de-prueba"), "", &[]);
        let datos: Value = serde_json::from_str(&cuerpo).unwrap();
        assert_eq!((codigo, datos["current"].clone(), datos["available"].clone()), (200, json!(update::CURRENT), Value::Null));
        assert_eq!(pedir(puerto, "POST", "/api/update/check", "evil.example", Some("token-de-prueba"), "{}", &[]).0, 403);
    }

    #[test]
    fn no_se_actualiza_con_un_trabajo_en_marcha() {
        let b = banco(vec![], 0, false);
        let app = App::with_job(&std::env::temp_dir(), b.job.clone(), "t");
        b.job.enqueue(formulario(1));
        let error = app.start_update_install().unwrap_err();
        assert!(error.contains("trabajo en marcha"), "{error}");
        b.job.stop();
        assert!(wait_for_status(&b.job, "stopped", Duration::from_secs(5)));
        assert!(app.start_update_install().is_ok(), "sin trabajo en marcha si se puede");
    }

    #[test]
    fn la_pagina_embebida_es_la_de_siempre() {
        assert!(INDEX_HTML.contains("const TOKEN = \"__TOKEN__\""));
        assert!(INDEX_HTML.contains("X-VS-Token") && INDEX_HTML.contains("/api/events"));
    }
}
