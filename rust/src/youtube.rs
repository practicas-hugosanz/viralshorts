//! Subir shorts a YouTube.
//!
//! YouTube solo deja subir videos a programas que se identifican con una
//! credencial propia (un "cliente OAuth" de Google Cloud, gratis). El usuario
//! la crea una vez y elige su fichero `client_secret_....json`; despues da
//! permiso en el navegador y aqui se guarda el permiso duradero (refresh token)
//! en `youtube.json`, dentro de la carpeta de datos del programa. A partir de
//! ahi cada subida pide un acceso de una hora con ese permiso.
//!
//! El permiso se pide con el flujo de aplicaciones de escritorio de Google:
//! una direccion de vuelta en 127.0.0.1 con un puerto libre, y PKCE para que
//! nadie que intercepte el codigo pueda canjearlo.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct YoutubeError(pub String);

type Result<T> = std::result::Result<T, YoutubeError>;

/// Las direcciones de Google. Son un dato para poder probar contra un
/// servidor de mentira.
#[derive(Debug, Clone)]
pub struct Endpoints {
    pub auth: String,
    pub token: String,
    pub api: String,
    pub upload: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            auth: "https://accounts.google.com/o/oauth2/v2/auth".into(),
            token: "https://oauth2.googleapis.com/token".into(),
            api: "https://www.googleapis.com/youtube/v3".into(),
            upload: "https://www.googleapis.com/upload/youtube/v3/videos".into(),
        }
    }
}

const SCOPES: &str = "https://www.googleapis.com/auth/youtube.upload https://www.googleapis.com/auth/youtube.readonly";
/// Lo que se espera a que el usuario de permiso en el navegador.
const CONSENT_TIMEOUT: Duration = Duration::from_secs(300);

fn settings_path(root: &Path) -> PathBuf {
    root.join("youtube.json")
}

fn load(root: &Path) -> Value {
    std::fs::read_to_string(settings_path(root)).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_else(|| json!({}))
}

fn save(root: &Path, data: &Value) -> Result<()> {
    std::fs::write(settings_path(root), serde_json::to_string_pretty(data).unwrap_or_default())
        .map_err(|e| YoutubeError(format!("No se pudo guardar la conexion con YouTube: {e}")))
}

/// Estado para la interfaz: si hay credencial, si hay permiso y de que canal.
pub fn status(root: &Path) -> Value {
    let data = load(root);
    json!({
        "has_client": data["client_id"].as_str().is_some_and(|s| !s.is_empty()),
        "connected": data["refresh_token"].as_str().is_some_and(|s| !s.is_empty()),
        "channel": data["channel"].as_str().unwrap_or(""),
    })
}

/// Olvida el permiso (la credencial se queda, para volver a conectar).
pub fn disconnect(root: &Path) -> Result<()> {
    let mut data = load(root);
    if let Some(map) = data.as_object_mut() {
        map.remove("refresh_token");
        map.remove("channel");
    }
    save(root, &data)
}

/// Lee el fichero de credencial que da Google Cloud ("Aplicacion de escritorio").
pub fn read_client_secret(path: &Path) -> Result<(String, String)> {
    let text = std::fs::read_to_string(path).map_err(|e| YoutubeError(format!("No se pudo leer {}: {e}", path.display())))?;
    let data: Value = serde_json::from_str(&text).map_err(|_| YoutubeError("Ese fichero no es una credencial de Google (JSON).".into()))?;
    let client = if data["installed"].is_object() { &data["installed"] } else { &data["web"] };
    match (client["client_id"].as_str(), client["client_secret"].as_str()) {
        (Some(id), Some(secret)) if !id.is_empty() && !secret.is_empty() => Ok((id.to_string(), secret.to_string())),
        _ => Err(YoutubeError("La credencial no trae client_id y client_secret. Crea una de tipo «Aplicacion de escritorio».".into())),
    }
}

/// base64 sin relleno y apto para URL (lo que pide PKCE).
fn base64url(bytes: &[u8]) -> String {
    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16) | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8) | u32::from(*chunk.get(2).unwrap_or(&0));
        for i in 0..(chunk.len() + 1) {
            out.push(ABC[((n >> (18 - 6 * i)) & 63) as usize] as char);
        }
    }
    out
}

fn random_text(bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    getrandom::fill(&mut buffer).expect("el sistema da bytes aleatorios");
    base64url(&buffer)
}

fn encode(value: &str) -> String {
    value.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

/// Direccion de consentimiento de Google.
pub fn consent_url(endpoints: &Endpoints, client_id: &str, redirect: &str, challenge: &str, state: &str) -> String {
    format!(
        "{}?client_id={}&redirect_uri={}&response_type=code&scope={}&code_challenge={challenge}&code_challenge_method=S256&state={state}&access_type=offline&prompt=consent",
        endpoints.auth,
        encode(client_id),
        encode(redirect),
        encode(SCOPES)
    )
}

/// Conecta con YouTube: guarda la credencial, abre el navegador para dar
/// permiso y espera a que Google vuelva. Devuelve el nombre del canal.
pub fn connect(root: &Path, client_secret: &Path, open: &dyn Fn(&str), endpoints: &Endpoints) -> Result<String> {
    let (client_id, secret) = read_client_secret(client_secret)?;
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| YoutubeError(format!("No se pudo esperar la respuesta de Google: {e}")))?;
    let redirect = format!("http://127.0.0.1:{}", listener.local_addr().map_err(|e| YoutubeError(e.to_string()))?.port());
    let verifier = random_text(48);
    let challenge = base64url(&Sha256::digest(verifier.as_bytes()));
    let state = random_text(16);
    open(&consent_url(endpoints, &client_id, &redirect, &challenge, &state));

    let code = wait_for_code(&listener, &state, CONSENT_TIMEOUT)?;
    let tokens = post_form(
        &endpoints.token,
        &[
            ("code", code.as_str()),
            ("client_id", client_id.as_str()),
            ("client_secret", secret.as_str()),
            ("redirect_uri", redirect.as_str()),
            ("grant_type", "authorization_code"),
            ("code_verifier", verifier.as_str()),
        ],
    )?;
    let refresh = tokens["refresh_token"].as_str().ok_or_else(|| YoutubeError("Google no devolvio el permiso duradero. Vuelve a intentarlo.".into()))?;
    let access = tokens["access_token"].as_str().unwrap_or("");
    let channel = channel_name(endpoints, access).unwrap_or_default();
    save(root, &json!({"client_id": client_id, "client_secret": secret, "refresh_token": refresh, "channel": channel}))?;
    Ok(channel)
}

/// Espera la vuelta de Google en `listener` y saca el codigo. Comprueba que
/// trae el mismo `state` que se mando: si no, no es la respuesta a lo nuestro.
pub fn wait_for_code(listener: &TcpListener, state: &str, timeout: Duration) -> Result<String> {
    listener.set_nonblocking(true).map_err(|e| YoutubeError(e.to_string()))?;
    let deadline = Instant::now() + timeout;
    loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                let _ = stream.set_nonblocking(false);
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                let mut first = String::new();
                let _ = BufReader::new(&stream).read_line(&mut first);
                let query = first.split_whitespace().nth(1).and_then(|p| p.split_once('?')).map(|(_, q)| q.to_string()).unwrap_or_default();
                let param = |name: &str| {
                    query.split('&').find_map(|kv| kv.split_once('=').filter(|(k, _)| *k == name).map(|(_, v)| decode(v)))
                };
                if param("code").is_none() && param("error").is_none() {
                    // El navegador tambien pide el favicon: se ignora.
                    let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                    continue;
                }
                let ok = param("state").as_deref() == Some(state) && param("code").is_some();
                let page = if ok {
                    "<h2>Listo</h2><p>Viral Shorts ya puede subir a tu canal. Puedes cerrar esta ventana.</p>"
                } else {
                    "<h2>No se ha conectado</h2><p>Vuelve a Viral Shorts e intentalo otra vez.</p>"
                };
                let body = format!("<!doctype html><meta charset=utf-8><body style=\"font:16px system-ui;margin:3em\">{page}</body>");
                let _ = stream.write_all(
                    format!("HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes(),
                );
                if let Some(error) = param("error") {
                    return Err(YoutubeError(if error == "access_denied" { "No diste permiso en Google.".into() } else { format!("Google respondio: {error}") }));
                }
                if !ok {
                    return Err(YoutubeError("La respuesta de Google no corresponde a esta conexion.".into()));
                }
                return Ok(param("code").unwrap_or_default());
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() > deadline {
                    return Err(YoutubeError("Se acabo el tiempo para dar permiso en el navegador (5 minutos).".into()));
                }
                std::thread::sleep(Duration::from_millis(150));
            }
            Err(e) => return Err(YoutubeError(e.to_string())),
        }
    }
}

fn decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                if let Ok(b) = u8::from_str_radix(&value[i + 1..i + 3], 16) {
                    out.push(b);
                    i += 3;
                    continue;
                }
                out.push(b'%');
            }
            b'+' => out.push(b' '),
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn post_form(url: &str, fields: &[(&str, &str)]) -> Result<Value> {
    match ureq::post(url).timeout(Duration::from_secs(30)).send_form(fields) {
        Ok(response) => response.into_json().map_err(|e| YoutubeError(format!("Respuesta rara de Google: {e}"))),
        Err(ureq::Error::Status(code, response)) => {
            let body: Value = response.into_json().unwrap_or(Value::Null);
            let detail = body["error_description"].as_str().or(body["error"].as_str()).unwrap_or("").to_string();
            Err(YoutubeError(format!("Google rechazo la peticion ({code}): {detail}")))
        }
        Err(error) => Err(YoutubeError(format!("Sin conexion con Google: {error}"))),
    }
}

fn channel_name(endpoints: &Endpoints, access: &str) -> Option<String> {
    let response = ureq::get(&format!("{}/channels?part=snippet&mine=true", endpoints.api))
        .set("Authorization", &format!("Bearer {access}"))
        .timeout(Duration::from_secs(20))
        .call()
        .ok()?;
    let data: Value = response.into_json().ok()?;
    data["items"][0]["snippet"]["title"].as_str().map(str::to_string)
}

/// Un acceso de una hora a partir del permiso guardado.
fn access_token(root: &Path, endpoints: &Endpoints) -> Result<String> {
    let data = load(root);
    let (Some(id), Some(secret), Some(refresh)) = (data["client_id"].as_str(), data["client_secret"].as_str(), data["refresh_token"].as_str()) else {
        return Err(YoutubeError("Conecta antes con YouTube en Ajustes.".into()));
    };
    let tokens = post_form(&endpoints.token, &[("client_id", id), ("client_secret", secret), ("refresh_token", refresh), ("grant_type", "refresh_token")])
        .map_err(|e| YoutubeError(format!("{e}. Si quitaste el permiso, vuelve a conectar en Ajustes.")))?;
    tokens["access_token"].as_str().map(str::to_string).ok_or_else(|| YoutubeError("Google no dio acceso.".into()))
}

/// Lo que se publica con el video.
#[derive(Debug, Clone, Default)]
pub struct Upload {
    pub title: String,
    pub description: String,
    pub tags: Vec<String>,
    /// public | unlisted | private
    pub privacy: String,
}

impl Upload {
    /// Los datos que manda la API. YouTube corta el titulo a 100 caracteres y
    /// no acepta < ni > en titulo y descripcion. Con #Shorts en la descripcion
    /// (y el video vertical de menos de 3 minutos) sale como Short.
    pub fn snippet(&self) -> Value {
        let clean = |s: &str| s.replace(['<', '>'], "");
        let title: String = clean(self.title.trim()).chars().take(100).collect();
        let hashtags: Vec<String> = self.tags.iter().map(|t| format!("#{}", t.trim_start_matches('#'))).collect();
        let mut description = clean(self.description.trim());
        if !hashtags.is_empty() {
            description.push_str("\n\n");
            description.push_str(&hashtags.join(" "));
        }
        if !description.to_lowercase().contains("#shorts") {
            description.push_str(" #Shorts");
        }
        let privacy = if ["public", "unlisted", "private"].contains(&self.privacy.as_str()) { self.privacy.as_str() } else { "private" };
        json!({
            "snippet": {"title": if title.is_empty() { "Short".to_string() } else { title }, "description": description.trim(), "tags": self.tags, "categoryId": "22"},
            "status": {"privacyStatus": privacy, "selfDeclaredMadeForKids": false},
        })
    }
}

/// Sube el video y devuelve su direccion en YouTube.
pub fn upload(root: &Path, video: &Path, details: &Upload, endpoints: &Endpoints) -> Result<String> {
    let access = access_token(root, endpoints)?;
    let size = std::fs::metadata(video).map_err(|e| YoutubeError(format!("No se pudo leer {}: {e}", video.display())))?.len();
    // Subida "reanudable": primero se anuncia el video y Google da una
    // direccion a la que mandar el fichero.
    let start = ureq::post(&format!("{}?uploadType=resumable&part=snippet,status", endpoints.upload))
        .set("Authorization", &format!("Bearer {access}"))
        .set("X-Upload-Content-Type", "video/mp4")
        .set("X-Upload-Content-Length", &size.to_string())
        .timeout(Duration::from_secs(30))
        .send_json(details.snippet());
    let location = match start {
        Ok(response) => response.header("Location").map(str::to_string),
        Err(ureq::Error::Status(code, response)) => return Err(api_error(code, response)),
        Err(error) => return Err(YoutubeError(format!("Sin conexion con YouTube: {error}"))),
    }
    .ok_or_else(|| YoutubeError("YouTube no dio donde subir el video.".into()))?;

    let file = std::fs::File::open(video).map_err(|e| YoutubeError(e.to_string()))?;
    let sent = ureq::put(&location)
        .set("Authorization", &format!("Bearer {access}"))
        .set("Content-Type", "video/mp4")
        .set("Content-Length", &size.to_string())
        .timeout(Duration::from_secs(1800))
        .send(file.take(size));
    let data: Value = match sent {
        Ok(response) => response.into_json().map_err(|e| YoutubeError(format!("Respuesta rara de YouTube: {e}")))?,
        Err(ureq::Error::Status(code, response)) => return Err(api_error(code, response)),
        Err(error) => return Err(YoutubeError(format!("Se corto la subida: {error}"))),
    };
    let id = data["id"].as_str().ok_or_else(|| YoutubeError("YouTube no devolvio el video subido.".into()))?;
    Ok(format!("https://youtube.com/shorts/{id}"))
}

fn api_error(code: u16, response: ureq::Response) -> YoutubeError {
    let body: Value = response.into_json().unwrap_or(Value::Null);
    let reason = body["error"]["errors"][0]["reason"].as_str().unwrap_or("");
    let message = body["error"]["message"].as_str().unwrap_or("");
    YoutubeError(match reason {
        "quotaExceeded" | "uploadLimitExceeded" => "YouTube no deja subir mas videos hoy con esta credencial (limite diario). Prueba manana.".into(),
        "youtubeSignupRequired" => "Esa cuenta de Google no tiene canal de YouTube. Crealo y vuelve a intentarlo.".into(),
        _ => format!("YouTube rechazo la subida ({code}): {message}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn base64url_sin_relleno() {
        assert_eq!(base64url(b"hola"), "aG9sYQ");
        assert_eq!(base64url(b""), "");
        assert_eq!(base64url(&[0xfb, 0xff]), "-_8");
    }

    #[test]
    fn la_credencial_de_escritorio_se_lee_y_la_mala_se_explica() {
        let dir = tempfile::tempdir().unwrap();
        let buena = dir.path().join("c.json");
        std::fs::write(&buena, r#"{"installed":{"client_id":"id.apps.googleusercontent.com","client_secret":"s3cr3t"}}"#).unwrap();
        assert_eq!(read_client_secret(&buena).unwrap(), ("id.apps.googleusercontent.com".into(), "s3cr3t".into()));
        let mala = dir.path().join("m.json");
        std::fs::write(&mala, r#"{"otra":"cosa"}"#).unwrap();
        assert!(read_client_secret(&mala).unwrap_err().0.contains("escritorio"));
    }

    #[test]
    fn el_permiso_lleva_pkce_estado_y_acceso_duradero() {
        let url = consent_url(&Endpoints::default(), "id&x", "http://127.0.0.1:5555", "RETO", "ESTADO");
        assert!(url.starts_with("https://accounts.google.com/o/oauth2/v2/auth?client_id=id%26x&"));
        assert!(url.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A5555") && url.contains("code_challenge=RETO&code_challenge_method=S256"));
        assert!(url.contains("state=ESTADO") && url.contains("access_type=offline") && url.contains("youtube.upload"));
    }

    #[test]
    fn la_vuelta_de_google_da_el_codigo_solo_con_el_mismo_estado() {
        let escucha = TcpListener::bind("127.0.0.1:0").unwrap();
        let puerto = escucha.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let pedir = |ruta: &str| {
                let mut s = std::net::TcpStream::connect(("127.0.0.1", puerto)).unwrap();
                s.write_all(format!("GET {ruta} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes()).unwrap();
                let mut r = String::new();
                let _ = s.read_to_string(&mut r);
                r
            };
            pedir("/favicon.ico");
            pedir("/?state=BIEN&code=4%2Fabc");
        });
        assert_eq!(wait_for_code(&escucha, "BIEN", Duration::from_secs(5)).unwrap(), "4/abc");

        let escucha = TcpListener::bind("127.0.0.1:0").unwrap();
        let puerto = escucha.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let mut s = std::net::TcpStream::connect(("127.0.0.1", puerto)).unwrap();
            s.write_all(b"GET /?state=OTRO&code=robado HTTP/1.1\r\n\r\n").unwrap();
        });
        assert!(wait_for_code(&escucha, "BIEN", Duration::from_secs(5)).is_err());
    }

    #[test]
    fn titulo_descripcion_y_hashtags_como_los_quiere_youtube() {
        let datos = Upload { title: "a<b>".repeat(40), description: "Pie.".into(), tags: vec!["humor".into(), "#minecraft".into()], privacy: "raro".into() }.snippet();
        assert_eq!(datos["snippet"]["title"].as_str().unwrap().chars().count(), 80, "sin < ni >");
        assert_eq!(datos["snippet"]["description"], "Pie.\n\n#humor #minecraft #Shorts");
        assert_eq!(datos["status"]["privacyStatus"], "private", "lo desconocido va en privado");
    }

    /// (codigo, cabeceras extra, cuerpo) de cada respuesta del servidor de mentira.
    type Respuesta = (u16, Vec<(&'static str, String)>, String);

    /// Servidor de mentira: responde en orden y apunta lo recibido.
    fn falso(respuestas: Vec<Respuesta>) -> (String, Arc<Mutex<Vec<String>>>) {
        let escucha = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", escucha.local_addr().unwrap());
        let recibido = Arc::new(Mutex::new(Vec::new()));
        let apunte = recibido.clone();
        std::thread::spawn(move || {
            for (codigo, cabeceras, cuerpo) in respuestas {
                let (mut s, _) = escucha.accept().unwrap();
                let mut datos = Vec::new();
                let mut trozo = [0u8; 65536];
                loop {
                    let n = s.read(&mut trozo).unwrap();
                    datos.extend_from_slice(&trozo[..n]);
                    if let Some(p) = datos.windows(4).position(|w| w == b"\r\n\r\n") {
                        let cab = String::from_utf8_lossy(&datos[..p]).to_lowercase();
                        let largo = cab.lines().find_map(|l| l.strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap())).unwrap_or(0);
                        while datos.len() < p + 4 + largo {
                            let n = s.read(&mut trozo).unwrap();
                            datos.extend_from_slice(&trozo[..n]);
                        }
                        break;
                    }
                }
                apunte.lock().unwrap().push(String::from_utf8_lossy(&datos).to_string());
                let extra: String = cabeceras.iter().map(|(k, v)| format!("{k}: {v}\r\n")).collect();
                let r = format!("HTTP/1.1 {codigo} X\r\nContent-Type: application/json\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{cuerpo}", cuerpo.len());
                s.write_all(r.as_bytes()).unwrap();
            }
        });
        (base, recibido)
    }

    #[test]
    fn subir_pide_acceso_anuncia_el_video_y_manda_el_fichero() {
        let dir = tempfile::tempdir().unwrap();
        save(dir.path(), &json!({"client_id": "id", "client_secret": "s", "refresh_token": "r", "channel": "Mi canal"})).unwrap();
        let video = dir.path().join("short_1.mp4");
        std::fs::write(&video, b"VIDEO DE PRUEBA").unwrap();
        // Tres peticiones: acceso y anuncio (que da la direccion de subida) a
        // "Google", y el fichero a esa direccion.
        let (destino, llegado) = falso(vec![(200, vec![], r#"{"id":"abc123"}"#.into())]);
        let (google, pedido) = falso(vec![
            (200, vec![], r#"{"access_token":"ACCESO"}"#.into()),
            (200, vec![("Location", format!("{destino}/sesion"))], "{}".into()),
        ]);
        let endpoints = Endpoints { auth: String::new(), token: format!("{google}/token"), api: String::new(), upload: format!("{google}/upload") };
        let detalles = Upload { title: "Titulo".into(), description: "Pie".into(), tags: vec!["a".into()], privacy: "unlisted".into() };
        assert_eq!(upload(dir.path(), &video, &detalles, &endpoints).unwrap(), "https://youtube.com/shorts/abc123");
        let pedido = pedido.lock().unwrap();
        assert!(pedido[0].contains("grant_type=refresh_token") && pedido[0].contains("refresh_token=r"));
        assert!(pedido[1].contains("uploadtype=resumable") || pedido[1].contains("uploadType=resumable"));
        assert!(pedido[1].contains("Bearer ACCESO") && pedido[1].contains("\"privacyStatus\":\"unlisted\""));
        assert!(llegado.lock().unwrap()[0].ends_with("VIDEO DE PRUEBA"));
    }

    #[test]
    fn sin_conectar_o_sin_cuota_el_error_se_entiende() {
        let dir = tempfile::tempdir().unwrap();
        let video = dir.path().join("v.mp4");
        std::fs::write(&video, b"x").unwrap();
        assert!(upload(dir.path(), &video, &Upload::default(), &Endpoints::default()).unwrap_err().0.contains("Conecta"));

        save(dir.path(), &json!({"client_id": "id", "client_secret": "s", "refresh_token": "r"})).unwrap();
        let (google, _) = falso(vec![
            (200, vec![], r#"{"access_token":"A"}"#.into()),
            (403, vec![], r#"{"error":{"errors":[{"reason":"quotaExceeded"}],"message":"quota"}}"#.into()),
        ]);
        let endpoints = Endpoints { auth: String::new(), token: format!("{google}/token"), api: String::new(), upload: format!("{google}/upload") };
        assert!(upload(dir.path(), &video, &Upload::default(), &endpoints).unwrap_err().0.contains("limite diario"));

        assert_eq!(status(dir.path())["connected"], true);
        disconnect(dir.path()).unwrap();
        assert_eq!((status(dir.path())["connected"].clone(), status(dir.path())["has_client"].clone()), (json!(false), json!(true)));
    }
}
