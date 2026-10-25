//! Lo que la interfaz guarda y consulta fuera del pipeline: la biblioteca de
//! shorts (todas las ejecuciones, no solo la ultima), los perfiles de ajustes y
//! los fotogramas de vista previa.

use std::path::{Component, Path, PathBuf};
use std::process::Stdio;

use serde_json::{json, Map, Value};

use crate::ffmpeg;
use crate::gui;

// ----------------------------------------------------------------------------
// Biblioteca de shorts
// ----------------------------------------------------------------------------

/// Una carpeta o un fichero que se puede pedir: solo letras, cifras y poco mas.
fn valid_part(part: &str) -> bool {
    !part.is_empty()
        && part != "."
        && part != ".."
        && !part.starts_with(' ')
        && part.chars().all(|c| c.is_alphanumeric() || matches!(c, '_' | '.' | '-' | ' ' | '(' | ')'))
}

/// Todos los shorts de `base` y de sus subcarpetas directas (una por ejecucion),
/// los mas recientes primero. `id` es la ruta relativa a `base`.
pub fn scan_library(base: &Path) -> Vec<Value> {
    let mut folders: Vec<(String, PathBuf)> = vec![(String::new(), base.to_path_buf())];
    if let Ok(entries) = std::fs::read_dir(base) {
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if path.is_dir() && valid_part(&name) {
                folders.push((name, path));
            }
        }
    }

    let mut all = Vec::new();
    for (name, path) in folders {
        for mut entry in gui::scan_output(&path) {
            let file = entry["fichero"].as_str().unwrap_or("").to_string();
            entry["carpeta"] = json!(name);
            entry["id"] = json!(if name.is_empty() { file.clone() } else { format!("{name}/{file}") });
            all.push(entry);
        }
    }
    all.sort_by(|a, b| {
        b["mtime"].as_u64().cmp(&a["mtime"].as_u64()).then_with(|| a["id"].as_str().cmp(&b["id"].as_str()))
    });
    all
}

/// El video que corresponde a un `id` de la biblioteca, si es de verdad un short
/// dentro de `base`. Nunca sale de `base`.
pub fn resolve_short(base: &Path, id: &str) -> Option<PathBuf> {
    let parts: Vec<&str> = id.split('/').collect();
    if parts.is_empty() || parts.len() > 2 || !parts.iter().all(|p| valid_part(p)) {
        return None;
    }
    let file = parts.last()?.to_lowercase();
    if !(file.starts_with("short_") && file.ends_with(".mp4")) {
        return None;
    }
    let path = parts.iter().fold(base.to_path_buf(), |acc, p| acc.join(p));
    let (canonical, root) = (path.canonicalize().ok()?, base.canonicalize().ok()?);
    (canonical.starts_with(&root) && canonical.is_file() && !canonical.components().any(|c| c == Component::ParentDir)).then_some(canonical)
}

/// Borra un short y todo lo que lo acompana (`.json`, portada...). Devuelve
/// cuantos ficheros quitó.
pub fn delete_short(base: &Path, id: &str) -> Result<usize, String> {
    let video = resolve_short(base, id).ok_or("Ese short no existe.")?;
    let folder = video.parent().ok_or("Ese short no existe.")?.to_path_buf();
    let stem = video.file_stem().ok_or("Ese short no existe.")?.to_string_lossy().into_owned();
    let mut removed = 0;
    for entry in std::fs::read_dir(&folder).map_err(|e| e.to_string())?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        // `short_1.` y no `short_1`: que `short_10` no caiga con `short_1`.
        if name.starts_with(&format!("{stem}.")) {
            std::fs::remove_file(entry.path()).map_err(|e| format!("No se pudo borrar {name}: {e}"))?;
            removed += 1;
        }
    }
    // La hoja de publicaciones se reconstruye con lo que queda.
    let sheet = folder.join("publicaciones.csv");
    if sheet.is_file() {
        let _ = std::fs::remove_file(&sheet);
        let _ = crate::exports::write_csv(&folder, "publicaciones.csv");
    }
    Ok(removed)
}

/// Cambia el titulo de publicacion de un short (el que se ve y se copia).
pub fn rename_short(base: &Path, id: &str, title: &str) -> Result<(), String> {
    let title = title.trim();
    if title.is_empty() {
        return Err("El título no puede estar vacío.".into());
    }
    let video = resolve_short(base, id).ok_or("Ese short no existe.")?;
    let sidecar = video.with_extension("json");
    let mut data: Value = std::fs::read_to_string(&sidecar).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_else(|| json!({}));
    if !data["publicacion"].is_object() {
        data["publicacion"] = json!({});
    }
    data["publicacion"]["title"] = json!(title.chars().take(120).collect::<String>());
    let text = serde_json::to_string_pretty(&data).map_err(|e| e.to_string())?;
    std::fs::write(&sidecar, text).map_err(|e| format!("No se pudo guardar el título: {e}"))?;
    if let Some(folder) = video.parent() {
        if folder.join("publicaciones.csv").is_file() {
            let _ = crate::exports::write_csv(folder, "publicaciones.csv");
        }
    }
    Ok(())
}

/// Sube un short a YouTube con su titulo, pie y hashtags, y apunta la
/// direccion en su .json para que la biblioteca la ensene.
pub fn upload_short(base: &Path, root: &Path, id: &str, privacy: &str, endpoints: &crate::youtube::Endpoints) -> Result<String, String> {
    let video = resolve_short(base, id).ok_or("Ese short no existe.")?;
    let sidecar = video.with_extension("json");
    let mut data: Value = std::fs::read_to_string(&sidecar).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_else(|| json!({}));
    let publication = &data["publicacion"];
    let details = crate::youtube::Upload {
        title: publication["title"].as_str().unwrap_or("").to_string(),
        description: publication["description"].as_str().unwrap_or("").to_string(),
        tags: publication["hashtags"].as_array().map(|t| t.iter().filter_map(Value::as_str).map(str::to_string).collect()).unwrap_or_default(),
        privacy: privacy.to_string(),
    };
    let url = crate::youtube::upload(root, &video, &details, endpoints).map_err(|e| e.to_string())?;
    data["youtube"] = json!({"url": url, "privacidad": details.snippet()["status"]["privacyStatus"]});
    let _ = std::fs::write(&sidecar, serde_json::to_string_pretty(&data).unwrap_or_default());
    Ok(url)
}

// ----------------------------------------------------------------------------
// Fotograma de vista previa
// ----------------------------------------------------------------------------

const PREVIEW_EXTENSIONS: [&str; 9] = ["mp4", "mkv", "webm", "mov", "m4v", "avi", "jpg", "jpeg", "png"];

/// Un fotograma JPEG pequeño de un video o imagen local. `None` si no se puede.
pub fn frame_jpeg(path: &Path) -> Option<Vec<u8>> {
    let ext = path.extension()?.to_string_lossy().to_lowercase();
    if !path.is_file() || !PREVIEW_EXTENSIONS.contains(&ext.as_str()) {
        return None;
    }
    // Un poco despues del principio (los primeros fotogramas suelen ser negros);
    // si el video es mas corto que eso, el primero.
    for seek in ["8", "0"] {
        let output = ffmpeg::command("ffmpeg")
            .args(["-v", "error", "-ss", seek, "-i"])
            .arg(path)
            .args(["-frames:v", "1", "-vf", "scale=540:-2", "-q:v", "5", "-f", "mjpeg", "-"])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        if output.status.success() && output.stdout.len() > 100 {
            return Some(output.stdout);
        }
    }
    None
}

// ----------------------------------------------------------------------------
// Perfiles de ajustes
// ----------------------------------------------------------------------------

const MAX_PROFILES: usize = 40;

fn profiles_path(root: &Path) -> PathBuf {
    root.join("profiles.json")
}

pub fn load_profiles(root: &Path) -> Map<String, Value> {
    std::fs::read_to_string(profiles_path(root))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

fn write_profiles(root: &Path, profiles: &Map<String, Value>) -> Result<(), String> {
    let path = profiles_path(root);
    let temp = path.with_extension("json.tmp");
    let text = serde_json::to_string_pretty(&Value::Object(profiles.clone())).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(root).map_err(|e| e.to_string())?;
    std::fs::write(&temp, text).map_err(|e| format!("No se pudo guardar el perfil: {e}"))?;
    std::fs::rename(&temp, &path).map_err(|e| format!("No se pudo guardar el perfil: {e}"))
}

pub fn save_profile(root: &Path, name: &str, data: &Value) -> Result<(), String> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 40 {
        return Err("El nombre del perfil tiene que tener entre 1 y 40 letras.".into());
    }
    if !data.is_object() || data.to_string().len() > 20_000 {
        return Err("Los ajustes del perfil no son válidos.".into());
    }
    let mut profiles = load_profiles(root);
    if !profiles.contains_key(name) && profiles.len() >= MAX_PROFILES {
        return Err(format!("Ya hay {MAX_PROFILES} perfiles. Borra alguno antes de crear otro."));
    }
    profiles.insert(name.to_string(), data.clone());
    write_profiles(root, &profiles)
}

pub fn delete_profile(root: &Path, name: &str) -> Result<(), String> {
    let mut profiles = load_profiles(root);
    if profiles.remove(name).is_none() {
        return Err("Ese perfil no existe.".into());
    }
    write_profiles(root, &profiles)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn short(dir: &Path, n: u32, titulo: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(format!("short_{n}.mp4")), b"video").unwrap();
        std::fs::write(dir.join(format!("short_{n}.jpg")), b"portada").unwrap();
        std::fs::write(
            dir.join(format!("short_{n}.json")),
            json!({"publicacion": {"title": titulo, "hook": "g", "hashtags": ["a"], "description": "d"}, "fuente": {"marca": "1:00"}}).to_string(),
        )
        .unwrap();
    }

    #[test]
    fn la_biblioteca_junta_todas_las_ejecuciones() {
        let dir = tempfile::tempdir().unwrap();
        short(dir.path(), 1, "suelto");
        short(&dir.path().join("pod-a_2026"), 1, "uno");
        short(&dir.path().join("pod-b_2026"), 2, "dos");
        std::fs::create_dir_all(dir.path().join("vacia")).unwrap();
        let lista = scan_library(dir.path());
        let ids: Vec<&str> = lista.iter().map(|e| e["id"].as_str().unwrap()).collect();
        assert_eq!(lista.len(), 3, "{ids:?}");
        assert!(ids.contains(&"short_1.mp4") && ids.contains(&"pod-a_2026/short_1.mp4") && ids.contains(&"pod-b_2026/short_2.mp4"));
        let uno = lista.iter().find(|e| e["id"] == "pod-a_2026/short_1.mp4").unwrap();
        assert_eq!(uno["carpeta"], "pod-a_2026");
        assert_eq!(uno["titulo"], "uno");
        assert!(uno["mtime"].as_u64().unwrap() > 0);
    }

    #[test]
    fn solo_se_resuelven_shorts_de_dentro() {
        let dir = tempfile::tempdir().unwrap();
        short(dir.path(), 1, "a");
        short(&dir.path().join("run"), 2, "b");
        assert!(resolve_short(dir.path(), "short_1.mp4").is_some());
        assert!(resolve_short(dir.path(), "run/short_2.mp4").is_some());
        for malo in ["", "../short_1.mp4", "run/../short_1.mp4", "a/b/short_1.mp4", "short_9.mp4", "run/otra.mp4", "C:/x/short_1.mp4", "short_1.json"] {
            assert!(resolve_short(dir.path(), malo).is_none(), "{malo}");
        }
    }

    #[test]
    fn borrar_quita_el_short_y_sus_acompanantes_pero_no_el_vecino() {
        let dir = tempfile::tempdir().unwrap();
        short(dir.path(), 1, "a");
        short(dir.path(), 10, "b");
        assert_eq!(delete_short(dir.path(), "short_1.mp4").unwrap(), 3);
        assert!(!dir.path().join("short_1.json").exists() && !dir.path().join("short_1.jpg").exists());
        assert!(dir.path().join("short_10.mp4").is_file() && dir.path().join("short_10.json").is_file(), "short_10 no es short_1");
        assert!(delete_short(dir.path(), "short_1.mp4").is_err(), "ya no existe");
    }

    #[test]
    fn renombrar_cambia_el_titulo_de_publicacion() {
        let dir = tempfile::tempdir().unwrap();
        short(dir.path(), 1, "viejo");
        rename_short(dir.path(), "short_1.mp4", "  Titulo nuevo ").unwrap();
        let lista = scan_library(dir.path());
        assert_eq!(lista[0]["titulo"], "Titulo nuevo");
        assert_eq!(lista[0]["gancho"], "g", "lo demas se conserva");
        assert!(rename_short(dir.path(), "short_1.mp4", "   ").is_err());
        assert!(rename_short(dir.path(), "short_7.mp4", "x").is_err());
    }

    #[test]
    fn los_perfiles_se_guardan_y_se_borran() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_profiles(dir.path()).is_empty());
        save_profile(dir.path(), "Humor", &json!({"clips": 5, "llm": true})).unwrap();
        save_profile(dir.path(), "Serio", &json!({"clips": 3})).unwrap();
        save_profile(dir.path(), "Humor", &json!({"clips": 7})).unwrap();
        let perfiles = load_profiles(dir.path());
        assert_eq!(perfiles.len(), 2);
        assert_eq!(perfiles["Humor"]["clips"], 7, "guardar con el mismo nombre sustituye");
        assert!(save_profile(dir.path(), "  ", &json!({})).is_err());
        assert!(save_profile(dir.path(), &"x".repeat(41), &json!({})).is_err());
        assert!(save_profile(dir.path(), "Mal", &json!("texto")).is_err());
        delete_profile(dir.path(), "Humor").unwrap();
        assert_eq!(load_profiles(dir.path()).len(), 1);
        assert!(delete_profile(dir.path(), "Humor").is_err());
    }

    #[test]
    fn sin_ffmpeg_o_con_un_fichero_raro_no_hay_fotograma() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"hola").unwrap();
        std::fs::write(dir.path().join("roto.mp4"), b"no es un video").unwrap();
        assert!(frame_jpeg(&dir.path().join("a.txt")).is_none(), "extension no permitida");
        assert!(frame_jpeg(&dir.path().join("no_existe.mp4")).is_none());
        assert!(frame_jpeg(&dir.path().join("roto.mp4")).is_none());
    }
}
