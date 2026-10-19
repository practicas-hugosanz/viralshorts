//! Hoja de calculo con los textos de publicacion de todos los shorts.
//!
//! Subir diez shorts es copiar y pegar diez titulos, diez descripciones y diez
//! listas de hashtags desde diez ficheros distintos. Esto los junta en un CSV que
//! se abre en Excel o en Google Sheets y sirve como calendario de publicacion.
//!
//! Se reconstruye a partir de los .json que el pipeline deja junto a cada video,
//! asi que incluye tambien los shorts de ejecuciones anteriores que sigan en la
//! carpeta.

use std::path::{Path, PathBuf};

use serde_json::Value;
use unicode_normalization::UnicodeNormalization;

const COLUMNS: [&str; 10] = [
    "fichero", "titulo", "gancho", "descripcion", "hashtags", "duracion", "minuto_original", "podcast",
    "url_origen", "puntuacion",
];

/// Carpeta propia de una ejecucion: `<base>/<titulo>_<tag>`.
///
/// Todos los shorts se llaman short_N.mp4, asi que sin una carpeta por
/// ejecucion el segundo podcast pisaba los shorts del primero.
pub fn run_folder(base: &Path, title: &str, tag: &str) -> PathBuf {
    let slug = slug(title);
    base.join(format!("{}_{tag}", if slug.is_empty() { "podcast" } else { &slug }))
}

fn slug(text: &str) -> String {
    let plain: String = text.nfkd().filter(|c| c.is_ascii()).collect::<String>().to_lowercase();
    let mut out = String::new();
    let mut pending_dash = false;
    for c in plain.chars() {
        if c.is_ascii_alphanumeric() {
            if pending_dash && !out.is_empty() {
                out.push('-');
            }
            pending_dash = false;
            out.push(c);
        } else {
            pending_dash = true;
        }
    }
    out.chars().take(40).collect::<String>().trim_end_matches('-').to_string()
}

/// Escribe (o rehace) el CSV de la carpeta de salida.
pub fn write_csv(output_dir: &Path, name: &str) -> Option<PathBuf> {
    let rows = rows(output_dir);
    if rows.is_empty() {
        return None;
    }

    let mut text = String::from('\u{feff}'); // sin el BOM, Excel en Windows abre las tildes rotas
    text.push_str(&line(&COLUMNS.map(String::from)));
    for row in &rows {
        text.push_str(&line(row));
    }

    let path = output_dir.join(name);
    if let Err(error) = std::fs::write(&path, text) {
        log::warn!("No se pudo escribir {name} ({error}).");
        return None;
    }
    log::info!("Textos de publicacion en {} ({} shorts).", path.display(), rows.len());
    Some(path)
}

fn rows(output_dir: &Path) -> Vec<[String; 10]> {
    let Ok(entries) = std::fs::read_dir(output_dir) else { return Vec::new() };
    let mut videos: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            let name = p.file_name().unwrap_or_default().to_string_lossy().to_lowercase();
            name.starts_with("short_") && name.ends_with(".mp4")
        })
        .collect();
    videos.sort();

    let text = |value: &Value| match value {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };

    let mut rows = Vec::new();
    for video in videos {
        let Ok(raw) = std::fs::read_to_string(video.with_extension("json")) else { continue };
        let Ok(data) = serde_json::from_str::<Value>(&raw) else { continue };
        let (publicacion, fuente) = (&data["publicacion"], &data["fuente"]);
        let hashtags: Vec<String> = publicacion["hashtags"]
            .as_array()
            .map(|tags| tags.iter().filter_map(Value::as_str).map(|t| format!("#{}", t.trim_start_matches('#'))).collect())
            .unwrap_or_default();
        rows.push([
            video.file_name().unwrap_or_default().to_string_lossy().into_owned(),
            text(&publicacion["title"]),
            text(&publicacion["hook"]),
            text(&publicacion["description"]),
            hashtags.join(" "),
            text(&data["duracion_final"]),
            text(&fuente["marca"]),
            text(&fuente["titulo"]),
            text(&fuente["url"]),
            text(&data["puntuacion"]["total"]),
        ]);
    }
    rows
}

/// Una linea CSV con ';' como separador y comillas solo donde hacen falta.
fn line(fields: &[String]) -> String {
    let quoted: Vec<String> = fields
        .iter()
        .map(|f| {
            if f.contains([';', '"', '\n', '\r']) { format!("\"{}\"", f.replace('"', "\"\"")) } else { f.clone() }
        })
        .collect();
    quoted.join(";") + "\r\n"
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn titulo_con_tildes_y_simbolos() {
        assert_eq!(run_folder(Path::new("output"), "¿Qué es la Inflación? | Ep. 12", "20260101_101500"), Path::new("output").join("que-es-la-inflacion-ep-12_20260101_101500"));
    }

    #[test]
    fn titulo_vacio_o_solo_simbolos() {
        for titulo in ["", "???", "日本語"] {
            assert_eq!(run_folder(Path::new("o"), titulo, "t").file_name().unwrap(), "podcast_t", "{titulo}");
        }
    }

    #[test]
    fn titulo_larguisimo_no_revienta_la_ruta() {
        let nombre = run_folder(Path::new("o"), &"palabra ".repeat(100), "t").file_name().unwrap().to_string_lossy().into_owned();
        assert!(nombre.chars().count() <= 42 && !nombre.starts_with('-'), "{nombre}");
        assert_ne!(run_folder(Path::new("o"), "Podcast A", "t"), run_folder(Path::new("o"), "Podcast B", "t"));
    }

    #[test]
    fn el_csv_recoge_los_textos_de_cada_short() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("short_1.mp4"), b"x").unwrap();
        std::fs::write(
            dir.path().join("short_1.json"),
            json!({
                "video": "short_1.mp4",
                "fuente": {"titulo": "El Programa", "url": "https://x", "marca": "12:30"},
                "duracion_final": 38.4,
                "puntuacion": {"total": 0.81},
                "publicacion": {"title": "Un titulo; con punto y coma", "hook": "UN \"GANCHO\"", "description": "Una descripcion.", "hashtags": ["podcast", "#clip"]},
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(dir.path().join("short_2.mp4"), b"x").unwrap(); // sin json: no sale

        let ruta = write_csv(dir.path(), "publicaciones.csv").unwrap();
        let bytes = std::fs::read(&ruta).unwrap();
        assert_eq!(&bytes[..3], &[0xEF, 0xBB, 0xBF], "BOM para Excel");
        let texto = String::from_utf8(bytes).unwrap();
        assert!(texto.contains("\"Un titulo; con punto y coma\""), "{texto}");
        assert!(texto.contains("\"UN \"\"GANCHO\"\"\""), "{texto}");
        assert!(texto.contains("#podcast #clip") && texto.contains("12:30") && texto.contains("38.4") && texto.contains("0.81"));
        assert_eq!(texto.lines().count(), 2, "cabecera y un short");
        assert!(texto.lines().next().unwrap().contains("fichero;titulo;gancho"));
    }

    #[test]
    fn sin_shorts_no_escribe_nada() {
        let dir = tempfile::tempdir().unwrap();
        assert!(write_csv(dir.path(), "publicaciones.csv").is_none());
        assert!(!dir.path().join("publicaciones.csv").exists());
        assert!(write_csv(&dir.path().join("no_existe"), "x.csv").is_none());
    }
}
