//! Descarga de ficheros por HTTP (modelos, binarios), con progreso.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::progress;

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct FetchError(pub String);

/// Descarga `url` en `dest` avisando por la barra de progreso del pipeline.
pub fn download_file(url: &str, dest: &Path, label: &str, stage: &str) -> Result<(), FetchError> {
    let mut last = Instant::now() - Duration::from_secs(1);
    download_file_with(url, dest, label, &mut |done, total| {
        let Some(total) = total.filter(|t| *t > 0) else { return };
        if done >= total {
            progress::progress(stage, 1.0, label, true);
        } else if last.elapsed() >= Duration::from_millis(500) {
            last = Instant::now();
            progress::progress(stage, done as f64 / total as f64, &format!("{label} {}/{} MB", done / 1_000_000, total / 1_000_000), false);
        }
    })
}

/// Descarga `url` en `dest`. Se escribe primero en `<dest>.part` y se renombra
/// al terminar: una descarga cortada nunca deja un fichero a medias que parezca
/// bueno. `on_progress` recibe (bytes bajados, bytes totales si se conocen).
pub fn download_file_with(url: &str, dest: &Path, label: &str, on_progress: &mut dyn FnMut(u64, Option<u64>)) -> Result<(), FetchError> {
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
    let copied = (|| -> Result<u64, String> {
        let mut file = File::create(&part).map_err(|e| e.to_string())?;
        let mut buffer = vec![0u8; 256 * 1024];
        let mut done: u64 = 0;
        loop {
            let read = reader.read(&mut buffer).map_err(|e| e.to_string())?;
            if read == 0 {
                break;
            }
            file.write_all(&buffer[..read]).map_err(|e| e.to_string())?;
            done += read as u64;
            on_progress(done, total);
        }
        file.flush().map_err(|e| e.to_string())?;
        Ok(done)
    })();

    // Una descarga que falla, o que se corta, no deja nada a medias.
    let done = match copied {
        Ok(done) => done,
        Err(error) => {
            let _ = fs::remove_file(&part);
            return Err(fail(error));
        }
    };
    if let Some(total) = total.filter(|t| *t != done) {
        let _ = fs::remove_file(&part);
        return Err(fail(format!("la descarga se corto ({done} de {total} bytes)")));
    }
    fs::rename(&part, dest).map_err(|e| fail(e.to_string()))?;
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

    /// Un servidor que sirve `cuerpo` y, si `cortar`, anuncia mas bytes de los que manda.
    fn servidor(cuerpo: Vec<u8>, anunciar: usize) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/f.bin", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            if let Ok((mut socket, _)) = listener.accept() {
                let mut peticion = [0u8; 2048];
                let _ = socket.read(&mut peticion);
                let cabecera = format!("HTTP/1.1 200 OK\r\nContent-Length: {anunciar}\r\nConnection: close\r\n\r\n");
                let _ = socket.write_all(cabecera.as_bytes());
                let _ = socket.write_all(&cuerpo);
            }
        });
        url
    }

    #[test]
    fn una_descarga_completa_se_guarda_y_avisa_del_progreso() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("sub").join("f.bin");
        let url = servidor(vec![7u8; 600_000], 600_000);
        let mut avisos = Vec::new();
        download_file_with(&url, &dest, "el fichero", &mut |hecho, total| avisos.push((hecho, total))).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap().len(), 600_000);
        assert_eq!(avisos.last(), Some(&(600_000, Some(600_000))));
        assert!(avisos.windows(2).all(|w| w[0].0 < w[1].0));
        assert!(!part_path(&dest).exists());
    }

    #[test]
    fn una_descarga_cortada_no_deja_nada_que_parezca_bueno() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("f.bin");
        let url = servidor(vec![1u8; 1000], 5000);
        let error = download_file_with(&url, &dest, "el fichero", &mut |_, _| {}).unwrap_err().to_string();
        assert!(!dest.exists() && !part_path(&dest).exists(), "{error}");
    }
}
