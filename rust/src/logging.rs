//! Registro de eventos por stderr, con el mismo aspecto que la version Python:
//! `HH:MM:SS NIVEL   modulo   mensaje`, con color si hay terminal.

use std::io::{IsTerminal, Write};

use log::{Level, LevelFilter, Log, Metadata, Record};

struct Logger {
    color: bool,
}

impl Log for Logger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        // Estas librerias son muy ruidosas en DEBUG.
        !(metadata.level() > Level::Warn && NOISY.iter().any(|n| metadata.target().starts_with(n)))
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let line = format_line(
            &chrono::Local::now().format("%H:%M:%S").to_string(),
            record.level(),
            record.target(),
            &record.args().to_string(),
            self.color,
        );
        // Una sola escritura: dos hilos no mezclan sus lineas.
        let _ = std::io::stderr().lock().write_all(line.as_bytes());
    }

    fn flush(&self) {}
}

const NOISY: [&str; 4] = ["hyper", "rustls", "ureq", "reqwest"];

pub fn format_line(time: &str, level: Level, target: &str, message: &str, color: bool) -> String {
    let name = match target.strip_prefix("viralshorts::") {
        Some(module) => format!("viralshorts.{module}"),
        None => target.to_string(),
    };
    let text = format!("{time} {:<7} {:<22} {message}", level_name(level), name);
    if !color {
        return text + "\n";
    }
    let paint = match level {
        Level::Trace | Level::Debug => "\x1b[38;5;244m",
        Level::Info => "\x1b[38;5;39m",
        Level::Warn => "\x1b[38;5;214m",
        Level::Error => "\x1b[38;5;196m",
    };
    format!("{paint}{text}\x1b[0m\n")
}

fn level_name(level: Level) -> &'static str {
    match level {
        Level::Trace | Level::Debug => "DEBUG",
        Level::Info => "INFO",
        Level::Warn => "WARNING",
        Level::Error => "ERROR",
    }
}

/// Instala el registro en stderr. Idempotente.
pub fn setup(verbose: bool) {
    let logger = Box::new(Logger { color: std::io::stderr().is_terminal() });
    if log::set_boxed_logger(logger).is_ok() {
        log::set_max_level(if verbose { LevelFilter::Debug } else { LevelFilter::Info });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linea_sin_color() {
        let linea = format_line("10:20:30", Level::Info, "viralshorts::render", "Listo", false);
        assert_eq!(linea, "10:20:30 INFO    viralshorts.render     Listo\n");
    }

    #[test]
    fn linea_con_color() {
        let linea = format_line("10:20:30", Level::Error, "viralshorts::x", "mal", true);
        assert!(linea.starts_with("\x1b[38;5;196m") && linea.ends_with("\x1b[0m\n"));
    }

    #[test]
    fn el_ruido_de_otras_librerias_solo_en_avisos() {
        let logger = Logger { color: false };
        let meta = |level, target| Metadata::builder().level(level).target(target).build();
        assert!(!logger.enabled(&meta(Level::Debug, "hyper::client")));
        assert!(logger.enabled(&meta(Level::Warn, "hyper::client")));
        assert!(logger.enabled(&meta(Level::Debug, "viralshorts::render")));
    }
}
