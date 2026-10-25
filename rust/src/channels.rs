//! Seguir canales: cuando un canal publica un episodio nuevo, se pone en la
//! cola solo, con los ajustes que habia al seguirlo.
//!
//! Se guarda en `channels.json` (carpeta de datos): el enlace, el nombre, los
//! ajustes del formulario y los videos ya vistos. Al seguir un canal sus
//! videos de ahora cuentan como vistos: solo se montan los que salgan despues,
//! no el historico entero.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

/// Lo que se mira de cada canal (los mas nuevos).
const LOOK_AT: usize = 6;
/// Un video mas corto que esto no es un episodio (avances, anuncios, shorts).
const MIN_EPISODE_SECONDS: f64 = 5.0 * 60.0;

/// Los ultimos videos de un canal: (id, titulo, segundos), del mas nuevo al mas viejo.
pub type Lister<'a> = &'a dyn Fn(&str) -> Result<Vec<(String, String, f64)>, String>;

fn path(root: &Path) -> PathBuf {
    root.join("channels.json")
}

pub fn load(root: &Path) -> Vec<Value> {
    std::fs::read_to_string(path(root)).ok().and_then(|t| serde_json::from_str::<Vec<Value>>(&t).ok()).unwrap_or_default()
}

fn save(root: &Path, channels: &[Value]) -> Result<(), String> {
    std::fs::write(path(root), serde_json::to_string_pretty(channels).unwrap_or_default()).map_err(|e| format!("No se pudo guardar la lista de canales: {e}"))
}

fn same_channel(a: &str, b: &str) -> bool {
    let key = |u: &str| crate::download::channel_videos_url(u).to_lowercase().replace("://www.", "://");
    key(a) == key(b)
}

/// Lo que ve la interfaz (sin los ajustes guardados ni la lista de vistos).
pub fn summary(root: &Path) -> Value {
    Value::Array(
        load(root)
            .iter()
            .map(|c| json!({"url": c["url"], "nombre": c["nombre"], "ultimo": c["ultimo"], "encolados": c["encolados"].as_u64().unwrap_or(0)}))
            .collect(),
    )
}

/// Empieza a seguir un canal. `form` son los ajustes con los que se montaran
/// sus episodios (el formulario de Crear tal cual, con su video de fondo).
pub fn follow(root: &Path, url: &str, form: &Value, list: Lister) -> Result<Value, String> {
    let url = url.trim();
    if !url.starts_with("https://") || !url.contains("youtube.com/") {
        return Err("Pega el enlace de un canal de YouTube (https://www.youtube.com/@canal).".into());
    }
    if form["background"].as_str().unwrap_or("").trim().is_empty() {
        return Err("Elige antes el video de fondo en Crear: los episodios se montaran con el.".into());
    }
    let mut channels = load(root);
    if channels.iter().any(|c| same_channel(c["url"].as_str().unwrap_or(""), url)) {
        return Err("Ya sigues ese canal.".into());
    }
    let videos = list(url)?;
    if videos.is_empty() {
        return Err("No se encontraron videos en ese canal. Comprueba el enlace.".into());
    }
    let name = channel_name(url);
    let entry = json!({
        "url": url,
        "nombre": name,
        "ajustes": form,
        "vistos": videos.iter().map(|(id, _, _)| id).collect::<Vec<_>>(),
        "ultimo": videos[0].1,
        "encolados": 0,
    });
    channels.push(entry.clone());
    save(root, &channels)?;
    Ok(json!({"url": url, "nombre": name, "ultimo": videos[0].1}))
}

pub fn unfollow(root: &Path, url: &str) -> Result<(), String> {
    let mut channels = load(root);
    let before = channels.len();
    channels.retain(|c| !same_channel(c["url"].as_str().unwrap_or(""), url));
    if channels.len() == before {
        return Err("No seguias ese canal.".into());
    }
    save(root, &channels)
}

/// "@nombre" sacado del enlace; si no, el enlace.
fn channel_name(url: &str) -> String {
    url.split('/').find(|p| p.starts_with('@')).map(str::to_string).unwrap_or_else(|| url.to_string())
}

/// Mira todos los canales. Devuelve un formulario por episodio nuevo (listo
/// para la cola) y apunta lo visto. Un canal que falla no para a los demas.
pub fn check(root: &Path, list: Lister) -> Vec<Value> {
    let mut channels = load(root);
    let mut jobs = Vec::new();
    for channel in channels.iter_mut() {
        let url = channel["url"].as_str().unwrap_or("").to_string();
        let videos = match list(&url) {
            Ok(videos) => videos,
            Err(error) => {
                log::warn!("No se pudo mirar el canal {url}: {error}");
                continue;
            }
        };
        let seen: Vec<String> = channel["vistos"].as_array().map(|v| v.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()).unwrap_or_default();
        // Del mas viejo al mas nuevo: la cola respeta el orden de publicacion.
        for (id, title, seconds) in videos.iter().rev().filter(|(id, _, _)| !seen.contains(id)) {
            if *seconds > 0.0 && *seconds < MIN_EPISODE_SECONDS {
                continue;
            }
            let mut form = channel["ajustes"].clone();
            form["podcast"] = json!(format!("https://www.youtube.com/watch?v={id}"));
            form["origen"] = json!(format!("{} · {title}", channel["nombre"].as_str().unwrap_or("")));
            jobs.push(form);
            channel["encolados"] = json!(channel["encolados"].as_u64().unwrap_or(0) + 1);
            log::info!("Episodio nuevo de {url}: {title}");
        }
        if let Some((_, title, _)) = videos.first() {
            channel["ultimo"] = json!(title);
        }
        // Se recuerdan los de ahora y algunos de antes (por si el canal reordena).
        let mut remembered: Vec<String> = videos.iter().map(|(id, _, _)| id.clone()).collect();
        let older: Vec<String> = seen.into_iter().filter(|id| !remembered.contains(id)).take(LOOK_AT * 4).collect();
        remembered.extend(older);
        channel["vistos"] = json!(remembered);
    }
    if let Err(error) = save(root, &channels) {
        log::warn!("{error}");
    }
    jobs
}

/// Los ultimos videos de verdad, con yt-dlp.
pub fn list_with_ytdlp(url: &str) -> Result<Vec<(String, String, f64)>, String> {
    crate::download::latest_videos(url, LOOK_AT).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn form() -> Value {
        json!({"podcast": "", "background": "juego.mp4", "clips": 3})
    }

    #[test]
    fn seguir_un_canal_no_monta_lo_que_ya_habia() {
        let dir = tempfile::tempdir().unwrap();
        let lista = |_: &str| Ok(vec![("b".into(), "Episodio 2".into(), 3600.0), ("a".into(), "Episodio 1".into(), 3600.0)]);
        let seguido = follow(dir.path(), "https://www.youtube.com/@MiPodcast", &form(), &lista).unwrap();
        assert_eq!((seguido["nombre"].as_str(), seguido["ultimo"].as_str()), (Some("@MiPodcast"), Some("Episodio 2")));
        assert!(check(dir.path(), &lista).is_empty(), "nada nuevo");
    }

    #[test]
    fn un_episodio_nuevo_va_a_la_cola_con_los_ajustes_de_entonces() {
        let dir = tempfile::tempdir().unwrap();
        let ahora = RefCell::new(vec![("a".to_string(), "Episodio 1".to_string(), 3600.0)]);
        let lista = |_: &str| Ok(ahora.borrow().clone());
        follow(dir.path(), "https://www.youtube.com/@MiPodcast/", &form(), &lista).unwrap();

        ahora.borrow_mut().insert(0, ("trailer".into(), "Avance".into(), 45.0));
        ahora.borrow_mut().insert(0, ("c".into(), "Episodio 3".into(), 5400.0));
        ahora.borrow_mut().insert(1, ("b".into(), "Episodio 2".into(), 4000.0));
        let trabajos = check(dir.path(), &lista);
        let enlaces: Vec<&str> = trabajos.iter().map(|t| t["podcast"].as_str().unwrap()).collect();
        assert_eq!(enlaces, ["https://www.youtube.com/watch?v=b", "https://www.youtube.com/watch?v=c"], "en orden y sin el avance");
        assert_eq!((trabajos[0]["background"].as_str(), trabajos[0]["clips"].as_i64()), (Some("juego.mp4"), Some(3)));
        assert!(check(dir.path(), &lista).is_empty(), "no se encola dos veces");
        assert_eq!(summary(dir.path())[0]["encolados"], 2);
        assert_eq!(summary(dir.path())[0]["ultimo"], "Episodio 3");
    }

    #[test]
    fn errores_claros_y_dejar_de_seguir() {
        let dir = tempfile::tempdir().unwrap();
        let lista = |_: &str| Ok(vec![("a".into(), "E".into(), 3600.0)]);
        assert!(follow(dir.path(), "hola", &form(), &lista).unwrap_err().contains("enlace"));
        assert!(follow(dir.path(), "https://www.youtube.com/@x", &json!({}), &lista).unwrap_err().contains("fondo"));
        follow(dir.path(), "https://www.youtube.com/@x", &form(), &lista).unwrap();
        assert!(follow(dir.path(), "https://youtube.com/@x/videos", &form(), &lista).unwrap_err().contains("Ya sigues"));
        let roto = |_: &str| Err::<Vec<(String, String, f64)>, String>("sin red".into());
        assert!(check(dir.path(), &roto).is_empty(), "un fallo no rompe nada");
        unfollow(dir.path(), "https://www.youtube.com/@x").unwrap();
        assert!(load(dir.path()).is_empty());
        assert!(unfollow(dir.path(), "https://www.youtube.com/@x").is_err());
    }

    #[test]
    fn la_pestana_de_videos_del_canal() {
        use crate::download::channel_videos_url;
        assert_eq!(channel_videos_url("https://www.youtube.com/@ibai/"), "https://www.youtube.com/@ibai/videos");
        assert_eq!(channel_videos_url("https://www.youtube.com/@ibai/streams"), "https://www.youtube.com/@ibai/streams");
        assert_eq!(channel_videos_url("https://www.youtube.com/playlist?list=X"), "https://www.youtube.com/playlist?list=X");
    }
}
