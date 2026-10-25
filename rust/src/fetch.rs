//! Descarga de ficheros por HTTP (modelos, binarios), con progreso.
//!
//! Los modelos de voz pesan hasta 3 GB y se bajan de un CDN que de vez en
//! cuando corta o se queda parado. Por eso una descarga:
//!
//! - se escribe en `<dest>.part` y solo se renombra al terminar, de modo que un
//!   fichero a medias nunca parece bueno;
//! - si se corta o se para, **sigue por donde iba** (cabecera Range) en vez de
//!   empezar de cero, unas cuantas veces antes de rendirse; y lo bajado se
//!   guarda para que el siguiente intento tambien siga por ahi;
//! - no se pisa con otra del mismo fichero (la interfaz y un trabajo en marcha
//!   pueden pedir el mismo modelo a la vez, desde procesos distintos): la
//!   segunda espera a la primera y ensena su progreso. Para eso hay un
//!   `<dest>.lock` que la que descarga refresca mientras avanza.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use crate::progress;

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct FetchError(pub String);

/// Intentos (con la conexion cortada o parada) antes de rendirse.
const ATTEMPTS: u32 = 6;
/// Sin recibir nada en este tiempo, la conexion se da por parada.
const STALL: Duration = Duration::from_secs(30);
/// Un `.lock` que no se ha refrescado en este tiempo es de una descarga muerta.
const STALE_LOCK: Duration = Duration::from_secs(90);

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

/// Descarga `url` en `dest`. `on_progress` recibe (bytes bajados, bytes
/// totales si se conocen).
pub fn download_file_with(url: &str, dest: &Path, label: &str, on_progress: &mut dyn FnMut(u64, Option<u64>)) -> Result<(), FetchError> {
    download_with_retries(url, dest, label, on_progress, &|attempt| Duration::from_secs(2u64.pow(attempt.min(4))))
}

fn download_with_retries(
    url: &str,
    dest: &Path,
    label: &str,
    on_progress: &mut dyn FnMut(u64, Option<u64>),
    pause: &dyn Fn(u32) -> Duration,
) -> Result<(), FetchError> {
    let fail = |what: String| FetchError(format!("No se pudo descargar {label} ({url}): {what}"));
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).map_err(|e| fail(e.to_string()))?;
    }

    // Otra descarga del mismo fichero en marcha: se espera a que acabe.
    let Some(lock) = Lock::take(dest, on_progress) else {
        return if dest.is_file() { Ok(()) } else { Err(fail("la otra descarga del mismo fichero no termino".into())) };
    };

    log::info!("Descargando {label}...");
    let part = part_path(dest);
    let mut last_error = String::new();
    for attempt in 0..ATTEMPTS {
        if attempt > 0 {
            log::warn!("La descarga de {label} se corto ({last_error}); se sigue por donde iba (intento {} de {ATTEMPTS}).", attempt + 1);
            std::thread::sleep(pause(attempt));
        }
        match fetch_once(url, &part, &lock, on_progress) {
            Ok(()) => {
                fs::rename(&part, dest).map_err(|e| fail(e.to_string()))?;
                return Ok(());
            }
            Err(Failure::Retry(error)) => last_error = error,
            Err(Failure::Fatal(error)) => {
                let _ = fs::remove_file(&part);
                return Err(fail(error));
            }
        }
    }
    // Lo bajado se queda en .part: el siguiente intento sigue desde ahi.
    Err(fail(format!("{last_error}. Lo descargado se guarda: al reintentar sigue por donde iba")))
}

enum Failure {
    /// Conexion cortada o parada: se reintenta desde lo ya bajado.
    Retry(String),
    /// No tiene arreglo reintentando (404, disco lleno...).
    Fatal(String),
}

/// Un intento: pide desde lo que ya hay en `.part` y anade lo que llegue.
fn fetch_once(url: &str, part: &Path, lock: &Lock, on_progress: &mut dyn FnMut(u64, Option<u64>)) -> Result<(), Failure> {
    let have = fs::metadata(part).map_or(0, |m| m.len());
    let agent = ureq::AgentBuilder::new().timeout_connect(Duration::from_secs(20)).timeout_read(STALL).user_agent("viralshorts").build();
    let mut request = agent.get(url);
    if have > 0 {
        request = request.set("Range", &format!("bytes={have}-"));
    }
    let response = match request.call() {
        Ok(response) => response,
        // Pedir mas alla del final: lo de .part no vale (otra version del fichero).
        Err(ureq::Error::Status(416, _)) => {
            let _ = fs::remove_file(part);
            return Err(Failure::Retry("el fichero cambio en el servidor; se empieza de nuevo".into()));
        }
        Err(ureq::Error::Status(code, _)) if (500..600).contains(&code) || code == 429 => return Err(Failure::Retry(format!("el servidor respondio {code}"))),
        Err(ureq::Error::Status(code, _)) => return Err(Failure::Fatal(format!("el servidor respondio {code}"))),
        Err(error) => return Err(Failure::Retry(error.to_string())),
    };

    // 206: sigue donde iba. 200: el servidor no sabe continuar, se empieza de cero.
    let resumed = response.status() == 206 && have > 0;
    let length: Option<u64> = response.header("Content-Length").and_then(|v| v.parse().ok());
    let total = if resumed {
        response
            .header("Content-Range")
            .and_then(|v| v.rsplit('/').next())
            .and_then(|v| v.parse().ok())
            .or(length.map(|l| l + have))
    } else {
        length
    };
    let mut file = if resumed {
        OpenOptions::new().append(true).open(part).map_err(|e| Failure::Fatal(e.to_string()))?
    } else {
        File::create(part).map_err(|e| Failure::Fatal(e.to_string()))?
    };
    let mut done = if resumed { have } else { 0 };
    on_progress(done, total);

    let mut reader = response.into_reader();
    let mut buffer = vec![0u8; 256 * 1024];
    loop {
        let read = match reader.read(&mut buffer) {
            Ok(read) => read,
            Err(error) => {
                let _ = file.flush();
                return Err(Failure::Retry(error.to_string()));
            }
        };
        if read == 0 {
            break;
        }
        // Escribir si puede fallar sin arreglo: disco lleno.
        file.write_all(&buffer[..read]).map_err(|e| Failure::Fatal(format!("no se pudo guardar ({e}); comprueba que queda espacio en el disco")))?;
        done += read as u64;
        on_progress(done, total);
        lock.touch(done, total);
    }
    file.flush().map_err(|e| Failure::Fatal(e.to_string()))?;
    drop(file);
    match total {
        Some(total) if total != done => Err(Failure::Retry(format!("se corto en {} de {} MB", done / 1_000_000, total / 1_000_000))),
        _ => Ok(()),
    }
}

/// Que solo un proceso baje cada fichero. El `.lock` lleva "bajados total" y
/// se refresca al avanzar; uno sin refrescar en `STALE_LOCK` es de una descarga
/// que murio y se puede tomar.
struct Lock {
    path: PathBuf,
    last_touch: std::cell::Cell<Instant>,
}

impl Lock {
    /// Toma el cerrojo, o espera a que la otra descarga acabe (ensenando su
    /// progreso) y devuelve None.
    fn take(dest: &Path, on_progress: &mut dyn FnMut(u64, Option<u64>)) -> Option<Lock> {
        let path = lock_path(dest);
        loop {
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(_) => return Some(Lock { path, last_touch: std::cell::Cell::new(Instant::now() - Duration::from_secs(60)) }),
                Err(_) => {
                    let age = fs::metadata(&path).and_then(|m| m.modified()).ok().and_then(|t| SystemTime::now().duration_since(t).ok());
                    match age {
                        // Desaparecio entre medias: se vuelve a intentar.
                        None => continue,
                        Some(age) if age > STALE_LOCK => {
                            log::warn!("Habia una descarga a medias abandonada de {}; se retoma.", dest.display());
                            let _ = fs::remove_file(&path);
                            continue;
                        }
                        Some(_) => {}
                    }
                    log::info!("Ya se esta descargando {}; se espera a que termine.", dest.display());
                    loop {
                        std::thread::sleep(Duration::from_millis(500));
                        let Ok(text) = fs::read_to_string(&path) else { break };
                        let mut numbers = text.split_whitespace().map(|n| n.parse::<u64>().ok());
                        if let (Some(Some(done)), total) = (numbers.next(), numbers.next().flatten()) {
                            on_progress(done, total);
                        }
                        let stale = fs::metadata(&path).and_then(|m| m.modified()).ok().and_then(|t| SystemTime::now().duration_since(t).ok()).is_some_and(|a| a > STALE_LOCK);
                        if stale {
                            break;
                        }
                    }
                    if dest.is_file() {
                        return None;
                    }
                    // La otra se murio sin terminar: se toma el relevo (sigue por su .part).
                    let _ = fs::remove_file(&path);
                }
            }
        }
    }

    /// Apunta el avance (como mucho una vez por segundo).
    fn touch(&self, done: u64, total: Option<u64>) {
        if self.last_touch.get().elapsed() < Duration::from_secs(1) {
            return;
        }
        self.last_touch.set(Instant::now());
        let _ = fs::write(&self.path, format!("{done} {}", total.map_or(String::new(), |t| t.to_string())));
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn part_path(dest: &Path) -> PathBuf {
    let mut name = dest.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    dest.with_file_name(name)
}

fn lock_path(dest: &Path) -> PathBuf {
    let mut name = dest.file_name().unwrap_or_default().to_os_string();
    name.push(".lock");
    dest.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn el_temporal_va_junto_al_destino() {
        assert_eq!(part_path(Path::new("bin/models/ggml-tiny.bin")), Path::new("bin/models/ggml-tiny.bin.part"));
    }

    #[test]
    fn una_url_que_no_responde_da_un_mensaje_con_el_motivo() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("x.bin");
        let error = download_with_retries("http://127.0.0.1:9/x.bin", &dest, "el modelo", &mut |_, _| {}, &|_| Duration::ZERO).unwrap_err().to_string();
        assert!(error.contains("el modelo") && error.contains("127.0.0.1"), "{error}");
        assert!(!dest.exists() && !lock_path(&dest).exists());
    }

    /// Servidor que sabe servir trozos (Range). `cortes` dice, para cada
    /// peticion, cuantos bytes manda antes de colgar (None = todos).
    fn servidor(cuerpo: Vec<u8>, cortes: Vec<Option<usize>>) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/f.bin", listener.local_addr().unwrap());
        let pedidos = Arc::new(Mutex::new(Vec::new()));
        let apunte = pedidos.clone();
        std::thread::spawn(move || {
            for corte in cortes {
                let Ok((mut socket, _)) = listener.accept() else { return };
                let mut peticion = [0u8; 4096];
                let n = socket.read(&mut peticion).unwrap_or(0);
                let texto = String::from_utf8_lossy(&peticion[..n]).to_lowercase();
                apunte.lock().unwrap().push(texto.clone());
                let desde: usize = texto.lines().find_map(|l| l.strip_prefix("range: bytes=").and_then(|r| r.trim_end_matches('-').parse().ok())).unwrap_or(0);
                let resto = &cuerpo[desde..];
                let cabecera = if desde > 0 {
                    format!("HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {desde}-{}/{}\r\nConnection: close\r\n\r\n", resto.len(), cuerpo.len() - 1, cuerpo.len())
                } else {
                    format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", cuerpo.len())
                };
                let _ = socket.write_all(cabecera.as_bytes());
                let _ = socket.write_all(&resto[..corte.unwrap_or(resto.len()).min(resto.len())]);
            }
        });
        (url, pedidos)
    }

    #[test]
    fn una_descarga_completa_se_guarda_y_avisa_del_progreso() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("sub").join("f.bin");
        let (url, _) = servidor(vec![7u8; 600_000], vec![None]);
        let mut avisos = Vec::new();
        download_file_with(&url, &dest, "el fichero", &mut |hecho, total| avisos.push((hecho, total))).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap().len(), 600_000);
        assert_eq!(avisos.last(), Some(&(600_000, Some(600_000))));
        assert!(!part_path(&dest).exists() && !lock_path(&dest).exists());
    }

    #[test]
    fn si_se_corta_sigue_por_donde_iba_y_no_empieza_de_cero() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("modelo.bin");
        let cuerpo: Vec<u8> = (0..1_000_000u32).map(|i| (i % 251) as u8).collect();
        // Se corta a los 300 KB y luego a los 200 KB mas; el tercero acaba.
        let (url, pedidos) = servidor(cuerpo.clone(), vec![Some(300_000), Some(200_000), None]);
        download_with_retries(&url, &dest, "el modelo", &mut |_, _| {}, &|_| Duration::ZERO).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), cuerpo, "el fichero sale entero y en orden");
        let pedidos = pedidos.lock().unwrap();
        assert!(!pedidos[0].contains("range:"));
        assert!(pedidos[1].contains("range: bytes=300000-"), "{}", pedidos[1]);
        assert!(pedidos[2].contains("range: bytes=500000-"), "{}", pedidos[2]);
    }

    #[test]
    fn rendirse_guarda_lo_bajado_para_el_siguiente_intento() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("f.bin");
        let cuerpo = vec![3u8; 100_000];
        let (url, _) = servidor(cuerpo.clone(), vec![Some(10_000); ATTEMPTS as usize]);
        let error = download_with_retries(&url, &dest, "el fichero", &mut |_, _| {}, &|_| Duration::ZERO).unwrap_err().to_string();
        assert!(error.contains("sigue por donde iba"), "{error}");
        assert!(!dest.exists(), "nada a medias con el nombre bueno");
        assert_eq!(std::fs::metadata(part_path(&dest)).unwrap().len(), 60_000, "seis trozos de 10 KB guardados");
        // El siguiente intento acaba lo que faltaba.
        let (url, pedidos) = servidor(cuerpo.clone(), vec![None]);
        download_with_retries(&url, &dest, "el fichero", &mut |_, _| {}, &|_| Duration::ZERO).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), cuerpo);
        assert!(pedidos.lock().unwrap()[0].contains("range: bytes=60000-"));
    }

    #[test]
    fn dos_descargas_del_mismo_fichero_no_se_pisan() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("modelo.bin");
        // Hay otra en marcha (cerrojo recien tocado): esta espera y, cuando la
        // otra deja el fichero, termina sin descargar nada.
        std::fs::write(lock_path(&dest), "500 1000").unwrap();
        let destino = dest.clone();
        let otra = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(1200));
            std::fs::write(&destino, b"listo").unwrap();
            std::fs::remove_file(lock_path(&destino)).unwrap();
        });
        let mut visto = Vec::new();
        download_with_retries("http://127.0.0.1:9/no-se-usa", &dest, "el modelo", &mut |hecho, total| visto.push((hecho, total)), &|_| Duration::ZERO)
            .unwrap();
        otra.join().unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"listo");
        assert!(visto.contains(&(500, Some(1000))), "ensena el progreso de la otra: {visto:?}");
    }

    #[test]
    fn un_cerrojo_abandonado_no_bloquea_para_siempre() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("f.bin");
        let viejo = lock_path(&dest);
        std::fs::write(&viejo, "1 2").unwrap();
        let hace_mucho = SystemTime::now() - Duration::from_secs(600);
        File::options().write(true).open(&viejo).unwrap().set_modified(hace_mucho).unwrap();
        let (url, _) = servidor(vec![9u8; 5000], vec![None]);
        download_with_retries(&url, &dest, "el fichero", &mut |_, _| {}, &|_| Duration::ZERO).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap().len(), 5000);
    }
}
