//! Actualizaciones: la aplicacion se mira sola si hay una version nueva y, si el
//! usuario quiere, se actualiza.
//!
//! Funciona como Arcia: el codigo vive en un repositorio privado y cada version
//! se publica como "release" en un repositorio PUBLICO solo de publicaciones
//! (`viralshorts-releases`), con dos ficheros:
//!
//!   viralshorts-setup-<version>.exe   el instalador
//!   latest.json                       version, notas, direccion y SHA-256
//!
//! La aplicacion lee `latest.json` de la ultima release (sin login ni limites de
//! la API de GitHub), y si la version es mayor que la suya ofrece actualizar:
//! baja el instalador, comprueba que no se ha corrompido, lo lanza en silencio y
//! se cierra para que pueda reemplazar el ejecutable. El instalador vuelve a
//! abrir la aplicacion al terminar.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::ffmpeg;
use crate::fetch;

/// Repositorio publico de publicaciones.
pub const RELEASES_REPO: &str = "practicas-hugosanz/viralshorts-releases";
/// Version de este programa (la de `Cargo.toml`).
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");

/// Direccion del `latest.json`. `VIRALSHORTS_UPDATE_URL` la sustituye (pruebas).
pub fn manifest_url() -> String {
    std::env::var("VIRALSHORTS_UPDATE_URL")
        .ok()
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| format!("https://github.com/{RELEASES_REPO}/releases/latest/download/latest.json"))
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Update {
    pub version: String,
    #[serde(default)]
    pub notes: String,
    pub url: String,
    #[serde(default)]
    pub sha256: Option<String>,
}

/// "v1.2.3" o "1.2.3" -> (1, 2, 3). Las versiones con sufijo ("1.2.3-beta") no
/// se aceptan: una prueba nunca se ofrece como actualizacion.
pub fn parse_version(text: &str) -> Option<(u64, u64, u64)> {
    let mut parts = text.trim().trim_start_matches(['v', 'V']).split('.');
    let mut next = || parts.next()?.parse::<u64>().ok();
    let version = (next()?, next()?, next()?);
    parts.next().is_none().then_some(version)
}

pub fn is_newer(candidate: &str, current: &str) -> bool {
    match (parse_version(candidate), parse_version(current)) {
        (Some(c), Some(k)) => c > k,
        _ => false,
    }
}

/// Que el instalador sale del repositorio de publicaciones (o de la direccion
/// de pruebas), no de cualquier sitio que diga el manifiesto.
fn url_allowed(url: &str, manifest: &str) -> bool {
    if url.starts_with(&format!("https://github.com/{RELEASES_REPO}/releases/download/")) {
        return true;
    }
    // Con la direccion de pruebas puesta, el instalador puede estar junto a ella.
    std::env::var("VIRALSHORTS_UPDATE_URL").is_ok() && url.starts_with(manifest.rsplit_once('/').map_or("", |(base, _)| base))
}

/// Mira si hay una version mas nueva que `current`.
pub fn check(manifest: &str, current: &str) -> Result<Option<Update>, String> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(10))
        .timeout_read(std::time::Duration::from_secs(20))
        .user_agent("viralshorts")
        .build();
    let body: Value = agent
        .get(manifest)
        .call()
        .map_err(|e| format!("No se pudo mirar si hay actualizaciones: {e}"))?
        .into_json()
        .map_err(|e| format!("El fichero de actualizaciones no se entiende: {e}"))?;
    let update: Update = serde_json::from_value(body).map_err(|e| format!("El fichero de actualizaciones no se entiende: {e}"))?;

    if parse_version(&update.version).is_none() {
        return Err(format!("Version rara en el fichero de actualizaciones: {}", update.version));
    }
    if !is_newer(&update.version, current) {
        return Ok(None);
    }
    if !url_allowed(&update.url, manifest) {
        return Err("La actualizacion apunta a una direccion que no es la de publicaciones de viralshorts; se ignora.".into());
    }
    Ok(Some(update))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// SHA-256 de un fichero, en hexadecimal minusculas.
pub fn sha256_file(path: &Path) -> std::io::Result<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 256 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex(&hasher.finalize()))
}

/// Baja el instalador a una carpeta temporal y comprueba su huella. Un
/// instalador a medias o cambiado nunca llega a ejecutarse.
pub fn download(update: &Update, on_progress: &mut dyn FnMut(u64, Option<u64>)) -> Result<PathBuf, String> {
    let folder = std::env::temp_dir().join("viralshorts-update");
    let _ = std::fs::remove_dir_all(&folder); // restos de una actualizacion anterior
    let dest = folder.join(format!("viralshorts-setup-{}.exe", update.version));
    fetch::download_file_with(&update.url, &dest, "la actualizacion", on_progress).map_err(|e| e.to_string())?;

    if let Some(expected) = update.sha256.as_deref().filter(|h| !h.is_empty()) {
        let actual = sha256_file(&dest).map_err(|e| e.to_string())?;
        if !actual.eq_ignore_ascii_case(expected.trim()) {
            let _ = std::fs::remove_file(&dest);
            return Err("La actualizacion se descargo mal (la huella no coincide). Vuelve a intentarlo.".into());
        }
    }
    Ok(dest)
}

/// Lanza el instalador sin esperarle: tiene que sobrevivir a esta aplicacion,
/// que se cierra para no tener su propio ejecutable en uso.
pub fn launch_installer(installer: &Path) -> std::io::Result<()> {
    let mut command = ffmpeg::command(installer);
    command.args(["/SILENT", "/SUPPRESSMSGBOXES", "/NORESTART", "/CLOSEAPPLICATIONS"]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP (y se quita CREATE_NO_WINDOW:
        // el instalador muestra su pequena ventana de progreso).
        command.creation_flags(0x0000_0008 | 0x0000_0200);
    }
    command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().map(|_| ())
}

// ----------------------------------------------------------------------------
// Estado para la interfaz
// ----------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct State {
    pub checked: bool,
    pub checking: bool,
    pub available: Option<Update>,
    /// "" | "descargando" | "instalando"
    pub phase: String,
    pub pct: f64,
    pub error: Option<String>,
}

pub type Shared = Mutex<State>;

impl State {
    pub fn to_json(&self) -> Value {
        json!({
            "current": CURRENT,
            "checked": self.checked,
            "checking": self.checking,
            "available": self.available,
            "phase": self.phase,
            "pct": self.pct,
            "error": self.error,
        })
    }
}

fn lock(state: &Shared) -> std::sync::MutexGuard<'_, State> {
    state.lock().unwrap_or_else(|e| e.into_inner())
}

/// Mira si hay version nueva y lo apunta en el estado.
pub fn run_check(state: &Shared, manifest: &str, current: &str) {
    {
        let mut s = lock(state);
        if s.checking || s.phase == "descargando" || s.phase == "instalando" {
            return;
        }
        s.checking = true;
        s.error = None;
    }
    let result = check(manifest, current);
    let mut s = lock(state);
    s.checking = false;
    s.checked = true;
    match result {
        Ok(found) => s.available = found,
        Err(error) => {
            log::info!("{error}");
            s.error = Some(error);
        }
    }
}

/// Baja la version que se ofrece, lanza el instalador y llama a `exit` para
/// cerrar la aplicacion. `launch` y `exit` se inyectan para poder probarlo.
pub fn run_install(state: &Shared, launch: &dyn Fn(&Path) -> std::io::Result<()>, exit: &dyn Fn()) {
    let update = {
        let mut s = lock(state);
        let Some(update) = s.available.clone() else { return };
        if !s.phase.is_empty() {
            return; // ya en marcha
        }
        s.phase = "descargando".into();
        s.pct = 0.0;
        s.error = None;
        update
    };

    let mut on_progress = |done: u64, total: Option<u64>| {
        if let Some(total) = total.filter(|t| *t > 0) {
            lock(state).pct = (done as f64 / total as f64).min(1.0);
        }
    };
    let installer = match download(&update, &mut on_progress) {
        Ok(path) => path,
        Err(error) => {
            let mut s = lock(state);
            s.phase.clear();
            s.error = Some(error);
            return;
        }
    };

    lock(state).phase = "instalando".into();
    match launch(&installer) {
        Ok(()) => exit(),
        Err(error) => {
            let mut s = lock(state);
            s.phase.clear();
            s.error = Some(format!("No se pudo abrir el instalador: {error}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    #[test]
    fn versiones() {
        assert_eq!(parse_version("1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version("v0.10.0"), Some((0, 10, 0)));
        for rara in ["", "1.2", "1.2.3.4", "1.2.3-beta", "a.b.c", "1..3"] {
            assert_eq!(parse_version(rara), None, "{rara}");
        }
    }

    #[test]
    fn solo_una_version_mayor_es_actualizacion() {
        assert!(is_newer("0.1.1", "0.1.0"));
        assert!(is_newer("0.10.0", "0.9.9"), "se compara como numero, no como texto");
        assert!(is_newer("1.0.0", "0.99.99"));
        for no in [("0.1.0", "0.1.0"), ("0.0.9", "0.1.0"), ("rara", "0.1.0"), ("0.2.0-beta", "0.1.0")] {
            assert!(!is_newer(no.0, no.1), "{no:?}");
        }
    }

    #[test]
    fn la_version_del_programa_es_valida() {
        assert!(parse_version(CURRENT).is_some(), "{CURRENT}");
    }

    #[test]
    fn el_instalador_tiene_que_salir_del_repositorio_de_publicaciones() {
        let _guard = ENTORNO.lock().unwrap_or_else(|e| e.into_inner());
        let manifiesto = format!("https://github.com/{RELEASES_REPO}/releases/latest/download/latest.json");
        assert!(url_allowed(&format!("https://github.com/{RELEASES_REPO}/releases/download/v1.0.0/viralshorts-setup-1.0.0.exe"), &manifiesto));
        for mala in [
            "https://evil.example/viralshorts-setup.exe",
            "https://github.com/otro/repo/releases/download/v1/setup.exe",
            "http://github.com/practicas-hugosanz/viralshorts-releases/releases/download/v1/setup.exe",
            "https://github.com/practicas-hugosanz/viralshorts-releases-falso/releases/download/v1/setup.exe",
        ] {
            assert!(!url_allowed(mala, &manifiesto), "{mala}");
        }
    }

    // -- un servidor de verdad con el manifiesto y el instalador ------------------------------

    /// Sirve `latest.json` en /latest.json y el instalador en /setup.exe.
    fn servidor(version: &str, instalador: Vec<u8>, sha: Option<String>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let manifiesto = json!({"version": version, "notes": "Mejoras", "url": format!("{base}/setup.exe"), "sha256": sha}).to_string();
        std::thread::spawn(move || {
            for conexion in listener.incoming().flatten() {
                let (manifiesto, instalador) = (manifiesto.clone(), instalador.clone());
                std::thread::spawn(move || {
                    let mut conexion = conexion;
                    let mut peticion = [0u8; 2048];
                    let leido = conexion.read(&mut peticion).unwrap_or(0);
                    let texto = String::from_utf8_lossy(&peticion[..leido]).into_owned();
                    let cuerpo: Vec<u8> = if texto.starts_with("GET /latest.json") { manifiesto.into_bytes() } else { instalador };
                    let cabecera = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", cuerpo.len());
                    let _ = conexion.write_all(cabecera.as_bytes()).and_then(|_| conexion.write_all(&cuerpo));
                });
            }
        });
        base
    }

    /// Las pruebas que tocan VIRALSHORTS_UPDATE_URL van de una en una.
    static ENTORNO: Mutex<()> = Mutex::new(());

    fn con_url<T>(url: &str, prueba: impl FnOnce() -> T) -> T {
        let _guard = ENTORNO.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("VIRALSHORTS_UPDATE_URL", url);
        let resultado = prueba();
        std::env::remove_var("VIRALSHORTS_UPDATE_URL");
        resultado
    }

    fn huella(datos: &[u8]) -> String {
        hex(&Sha256::digest(datos))
    }

    #[test]
    fn sin_version_nueva_no_se_ofrece_nada() {
        let base = servidor("0.1.0", vec![], None);
        let url = format!("{base}/latest.json");
        assert_eq!(con_url(&url, || check(&url, "0.1.0")).unwrap(), None);
        assert_eq!(con_url(&url, || check(&url, "0.2.0")).unwrap(), None);
    }

    #[test]
    fn con_version_nueva_se_ofrece_con_sus_notas() {
        let base = servidor("0.2.0", b"x".to_vec(), None);
        let url = format!("{base}/latest.json");
        let oferta = con_url(&url, || check(&url, "0.1.0")).unwrap().expect("hay version nueva");
        assert_eq!((oferta.version.as_str(), oferta.notes.as_str()), ("0.2.0", "Mejoras"));
        assert!(oferta.url.ends_with("/setup.exe"));
    }

    #[test]
    fn sin_conexion_o_con_basura_el_error_es_claro_y_no_rompe() {
        assert!(check("http://127.0.0.1:9/latest.json", "0.1.0").unwrap_err().contains("actualizaciones"));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/latest.json", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            if let Ok((mut c, _)) = listener.accept() {
                let mut b = [0u8; 1024];
                let _ = c.read(&mut b);
                let _ = c.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\nConnection: close\r\n\r\nno es json");
            }
        });
        assert!(check(&url, "0.1.0").unwrap_err().contains("no se entiende"));
    }

    #[test]
    fn un_manifiesto_que_apunta_fuera_se_ignora() {
        // Sin la direccion de pruebas, solo vale el repositorio de publicaciones.
        let _guard = ENTORNO.lock().unwrap_or_else(|e| e.into_inner());
        let base = servidor("0.2.0", vec![], None);
        let error = check(&format!("{base}/latest.json"), "0.1.0").unwrap_err();
        assert!(error.contains("no es la de publicaciones"), "{error}");
    }

    #[test]
    fn la_descarga_se_verifica_con_su_huella() {
        let instalador = vec![42u8; 300_000];
        let base = servidor("0.2.0", instalador.clone(), Some(huella(&instalador)));
        let url = format!("{base}/latest.json");
        con_url(&url, || {
            let oferta = check(&url, "0.1.0").unwrap().unwrap();
            let mut avisos = 0;
            let ruta = download(&oferta, &mut |_, _| avisos += 1).unwrap();
            assert_eq!(std::fs::read(&ruta).unwrap(), instalador);
            assert!(ruta.ends_with("viralshorts-setup-0.2.0.exe") && avisos > 0);

            let mala = Update { sha256: Some(huella(b"otra cosa")), ..oferta };
            let error = download(&mala, &mut |_, _| {}).unwrap_err();
            assert!(error.contains("huella"), "{error}");
            assert!(!ruta.exists(), "un instalador que no coincide se borra");
        });
    }

    #[test]
    fn el_flujo_completo_descarga_lanza_el_instalador_y_se_cierra() {
        let instalador = vec![7u8; 100_000];
        let base = servidor("0.3.0", instalador.clone(), Some(huella(&instalador)));
        let url = format!("{base}/latest.json");
        con_url(&url, || {
            let estado: Shared = Mutex::new(State::default());
            run_check(&estado, &url, "0.1.0");
            assert!(lock(&estado).checked && lock(&estado).available.is_some());

            let lanzado: Arc<Mutex<Option<PathBuf>>> = Arc::default();
            let cerrado = Arc::new(Mutex::new(false));
            let (l, c) = (lanzado.clone(), cerrado.clone());
            run_install(&estado, &move |ruta| { *l.lock().unwrap() = Some(ruta.to_path_buf()); Ok(()) }, &move || *c.lock().unwrap() = true);

            assert_eq!(std::fs::read(lanzado.lock().unwrap().as_ref().unwrap()).unwrap(), instalador);
            assert!(*cerrado.lock().unwrap(), "la aplicacion se cierra para dejar reemplazar el .exe");
            assert_eq!(lock(&estado).phase, "instalando");
            assert!(lock(&estado).error.is_none());
        });
    }

    #[test]
    fn si_el_instalador_no_se_puede_abrir_la_aplicacion_no_se_cierra() {
        let base = servidor("0.3.0", vec![1u8; 1000], None);
        let url = format!("{base}/latest.json");
        con_url(&url, || {
            let estado: Shared = Mutex::new(State::default());
            run_check(&estado, &url, "0.1.0");
            let cerrado = Arc::new(Mutex::new(false));
            let c = cerrado.clone();
            run_install(&estado, &|_| Err(std::io::Error::other("bloqueado")), &move || *c.lock().unwrap() = true);
            assert!(!*cerrado.lock().unwrap());
            let s = lock(&estado);
            assert!(s.phase.is_empty() && s.error.as_deref().unwrap_or("").contains("bloqueado"));
        });
    }

    #[test]
    fn sin_oferta_instalar_no_hace_nada_y_el_estado_se_cuenta_en_json() {
        let estado: Shared = Mutex::new(State::default());
        run_install(&estado, &|_| panic!("no hay nada que lanzar"), &|| panic!("no hay que cerrar"));
        let datos = lock(&estado).to_json();
        assert_eq!(datos["current"], CURRENT);
        assert_eq!((datos["checked"].clone(), datos["available"].clone()), (json!(false), Value::Null));
    }

    #[test]
    fn la_huella_de_un_fichero() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.bin");
        std::fs::write(&f, b"abc").unwrap();
        assert_eq!(sha256_file(&f).unwrap(), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert!(sha256_file(&dir.path().join("no.bin")).is_err());
    }
}
