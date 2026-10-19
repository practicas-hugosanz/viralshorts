//! Canal de progreso, con dos formatos segun quien este mirando.
//!
//! En una terminal se pinta una barra con retorno de carro. Cuando el pipeline
//! lo lanza la interfaz (variable de entorno `VIRALSHORTS_PROGRESS=json`) se
//! emiten lineas JSON con el prefijo `@@VS `, que la interfaz parsea para mover
//! sus barras. Sin ninguna de las dos cosas no se escribe nada.

use std::collections::HashSet;
use std::io::{IsTerminal, Write};
use std::sync::{Mutex, OnceLock};

use serde_json::{json, Value};

pub const PREFIX: &str = "@@VS ";
const BAR_WIDTH: usize = 28;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Tty,
    Json,
    Off,
}

impl Mode {
    /// Decide el formato a partir de la variable de entorno y del terminal.
    pub fn resolve(requested: Option<&str>, stderr_is_terminal: bool) -> Mode {
        match requested.map(str::to_lowercase).as_deref() {
            Some("json") => Mode::Json,
            Some("off") => Mode::Off,
            Some("tty") => Mode::Tty,
            _ if stderr_is_terminal => Mode::Tty,
            _ => Mode::Off,
        }
    }
}

pub fn mode() -> Mode {
    static MODE: OnceLock<Mode> = OnceLock::new();
    *MODE.get_or_init(|| {
        let requested = std::env::var("VIRALSHORTS_PROGRESS").ok();
        Mode::resolve(requested.as_deref(), std::io::stderr().is_terminal())
    })
}

fn suppressed() -> &'static Mutex<HashSet<String>> {
    static SET: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    SET.get_or_init(Default::default)
}

/// Marca el comienzo de una fase del pipeline.
pub fn stage(name: &str, label: &str) {
    if mode() == Mode::Json {
        write_json(&json!({"type": "stage", "stage": name, "label": label}));
    }
}

/// Silencia el avance de una fase. Con varios renders a la vez cada ffmpeg
/// reporta su propio porcentaje y la barra saltaria hacia atras.
pub fn suppress(stage_name: &str) {
    suppressed().lock().unwrap().insert(stage_name.to_string());
}

pub fn allow(stage_name: &str) {
    suppressed().lock().unwrap().remove(stage_name);
}

/// Avance dentro de una fase. `fraction` va de 0 a 1.
pub fn progress(stage_name: &str, fraction: f64, label: &str, last: bool) {
    let current = mode();
    if current == Mode::Off || suppressed().lock().unwrap().contains(stage_name) {
        return;
    }
    let fraction = fraction.clamp(0.0, 1.0);
    if current == Mode::Json {
        write_json(&progress_event(stage_name, fraction, label));
        return;
    }
    let mut err = std::io::stderr().lock();
    let _ = write!(err, "\r{}", bar_line(if label.is_empty() { stage_name } else { label }, fraction));
    if last {
        let _ = writeln!(err);
    }
    let _ = err.flush();
}

/// Evento estructurado para la interfaz (clip elegido, fichero generado...).
/// `data` debe ser un objeto; se le anade `"type": kind`.
pub fn result(kind: &str, data: Value) {
    if mode() == Mode::Json {
        write_json(&result_event(kind, data));
    }
}

pub fn progress_event(stage_name: &str, fraction: f64, label: &str) -> Value {
    json!({
        "type": "progress",
        "stage": stage_name,
        "pct": (fraction.clamp(0.0, 1.0) * 1000.0).round() / 10.0,
        "label": label,
    })
}

pub fn result_event(kind: &str, data: Value) -> Value {
    let mut event = serde_json::Map::new();
    event.insert("type".into(), Value::String(kind.to_string()));
    if let Value::Object(fields) = data {
        event.extend(fields);
    }
    Value::Object(event)
}

pub fn bar_line(label: &str, fraction: f64) -> String {
    let fraction = fraction.clamp(0.0, 1.0);
    let filled = (fraction * BAR_WIDTH as f64) as usize;
    format!(
        "  {label} [{}{}] {:5.1}%",
        "#".repeat(filled),
        "-".repeat(BAR_WIDTH - filled),
        fraction * 100.0
    )
}

fn write_json(payload: &Value) {
    // Una sola llamada a write: dos hilos no mezclan sus lineas.
    let line = format!("{PREFIX}{payload}\n");
    let mut err = std::io::stderr().lock();
    let _ = err.write_all(line.as_bytes());
    let _ = err.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn el_modo_sale_de_la_variable_y_del_terminal() {
        assert_eq!(Mode::resolve(Some("json"), true), Mode::Json);
        assert_eq!(Mode::resolve(Some("JSON"), false), Mode::Json);
        assert_eq!(Mode::resolve(Some("off"), true), Mode::Off);
        assert_eq!(Mode::resolve(Some("tty"), false), Mode::Tty);
        assert_eq!(Mode::resolve(None, true), Mode::Tty);
        assert_eq!(Mode::resolve(Some("auto"), false), Mode::Off);
    }

    #[test]
    fn el_evento_de_resultado_lleva_el_tipo_y_los_datos() {
        let e = result_event("salida", json!({"ruta": "C:/x"}));
        assert_eq!(e["type"], "salida");
        assert_eq!(e["ruta"], "C:/x");
    }

    #[test]
    fn el_porcentaje_se_redondea_a_un_decimal_y_se_acota() {
        assert_eq!(progress_event("render", 0.12345, "")["pct"], 12.3);
        assert_eq!(progress_event("render", 2.0, "")["pct"], 100.0);
        assert_eq!(progress_event("render", -1.0, "")["pct"], 0.0);
    }

    #[test]
    fn la_barra_mide_siempre_lo_mismo() {
        for f in [0.0, 0.5, 1.0, 7.0] {
            let linea = bar_line("render", f);
            let dentro = linea.split('[').nth(1).unwrap().split(']').next().unwrap();
            assert_eq!(dentro.len(), BAR_WIDTH);
        }
        assert!(bar_line("x", 0.5).ends_with(" 50.0%"));
    }
}
