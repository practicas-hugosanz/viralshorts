//! Descarga de ficheros por HTTP (modelos, binarios), con progreso.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::progress;

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct FetchError(pub String);

/// Descarga `url` en `dest`. Se escribe primero en `<dest>.part` y se renombra
/// al terminar: una descarga cortada nunca deja un fichero a medias que parezca
/// bueno.
pub fn download_file(url: &str, dest: &Path, label: &str, stage: &str) -> Result<(), FetchError> {
    let fail = |what: String| FetchError(format!("No se pudo descargar {label} ({url}): {what}"));

    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).map_err(|e| fail(e.to_string()))?;
    }
    let part = part_path(dest);

    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(20))
        .timeout_read(Duration::from_secs(60))
        .user_agent("viralshorts")
        .build();
    let response = agent.get(url).call().map_err(|e| fail(e.to_string()))?;
    let total: Option<u64> = response.header("Content-Length").and_then(|v| v.parse().ok());

    log::info!("Descargando {label}...");
    let mut reader = response.into_reader();
    let mut file = File::create(&part).map_err(|e| fail(e.to_string()))?;
    let mut buffer = vec![0u8; 256 * 1024];
    let mut done: u64 = 0;
    let mut last_report = Instant::now();
    loop {
        let read = reader.read(&mut buffer).map_err(|e| fail(e.to_string()))?;
        if read == 0 {
            break;
        }
        file.write_all(&buffer[..read]).map_err(|e| fail(e.to_string()))?;
        done += read as u64;
        if let Some(total) = total.filter(|t| *t > 0) {
            if last_report.elapsed() >= Duration::from_millis(500) {
                last_report = Instant::now();
                progress::progress(
                    stage,
                    done as f64 / total as f64,
                    &format!("{label} {}/{} MB", done / 1_000_000, total / 1_000_000),
                    false,
                );
            }
        }
    }
    file.flush().map_err(|e| fail(e.to_string()))?;
    drop(file);

    if let Some(total) = total {
        if done != total {
            let _ = fs::remove_file(&part);
            return Err(fail(format!("la descarga se corto ({done} de {total} bytes)")));
        }
    }
    fs::rename(&part, dest).map_err(|e| fail(e.to_string()))?;
    progress::progress(stage, 1.0, label, true);
    Ok(())
}

fn part_path(dest: &Path) -> PathBuf {
    let mut name = dest.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    dest.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn el_temporal_va_junto_al_destino() {
        assert_eq!(part_path(Path::new("bin/models/ggml-tiny.bin")), Path::new("bin/models/ggml-tiny.bin.part"));
    }

    #[test]
    fn una_url_que_no_responde_da_un_mensaje_con_el_motivo() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("x.bin");
        let error = download_file("http://127.0.0.1:9/x.bin", &dest, "el modelo", "modelo").unwrap_err().to_string();
        assert!(error.contains("el modelo") && error.contains("127.0.0.1"), "{error}");
        assert!(!dest.exists() && !part_path(&dest).exists());
    }
}
