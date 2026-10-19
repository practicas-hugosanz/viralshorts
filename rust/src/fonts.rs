//! Comprobacion de que la fuente de los subtitulos existe en el sistema.
//!
//! libass no avisa cuando no encuentra una fuente: la sustituye por la que sea
//! y sigue. El resultado es un short entero con subtitulos que no se parecen en
//! nada a los configurados. Aqui se comprueba ANTES de renderizar, y de forma
//! deliberadamente optimista: si no se puede saber, se dice que si esta y no se
//! molesta al usuario con una alarma falsa.

use std::collections::HashSet;
#[cfg(not(windows))]
use std::path::PathBuf;
use std::sync::OnceLock;

fn normalise(name: &str) -> String {
    name.to_lowercase().chars().filter(|c| c.is_alphanumeric()).collect()
}

/// Nombres de familia normalizados que hay instalados. Vacio = no se sabe.
pub fn installed_families() -> &'static HashSet<String> {
    static CACHE: OnceLock<HashSet<String>> = OnceLock::new();
    CACHE.get_or_init(scan)
}

/// True si la fuente se puede usar (o si no hay forma de comprobarlo).
pub fn is_available(font_name: &str, fonts_dir: Option<&str>) -> bool {
    is_available_in(font_name, fonts_dir, installed_families())
}

pub fn is_available_in(font_name: &str, fonts_dir: Option<&str>, families: &HashSet<String>) -> bool {
    if font_name.is_empty() {
        return true;
    }
    let wanted = normalise(font_name);

    if let Some(dir) = fonts_dir.filter(|d| !d.is_empty()) {
        // Con fontsdir libass carga los .ttf de esa carpeta aunque no esten
        // instalados; comprobarlo por nombre de fichero es aproximado pero
        // suficiente para no dar una alarma falsa.
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
                if ["ttf", "otf", "ttc"].contains(&ext.as_str())
                    && normalise(&path.file_stem().unwrap_or_default().to_string_lossy()).contains(&wanted)
                {
                    return true;
                }
            }
        }
    }

    if families.is_empty() {
        return true; // no se sabe: no alarmar
    }
    families.iter().any(|family| *family == wanted || family.starts_with(&wanted))
}

/// Fuente instalada que mas se parezca a la pedida, para el aviso.
pub fn suggest_alternative(_font_name: &str) -> String {
    suggest_in(installed_families())
}

pub fn suggest_in(families: &HashSet<String>) -> String {
    for candidate in [
        "Arial Black", "Impact", "Montserrat ExtraBold", "Anton", "Bebas Neue", "Verdana Bold", "Arial",
        "DejaVu Sans Bold", "Liberation Sans Bold",
    ] {
        if families.contains(&normalise(candidate)) {
            return candidate.to_string();
        }
    }
    "Arial".into()
}

/// 'Arial Black Bold Italic' -> ['Arial Black Bold Italic', ..., 'Arial'].
pub fn family_variants(name: &str) -> Vec<String> {
    let mut parts: Vec<&str> = name.split(|c: char| c == '&' || c.is_whitespace()).filter(|p| !p.is_empty()).collect();
    let mut variants = Vec::new();
    while !parts.is_empty() {
        variants.push(parts.join(" "));
        parts.pop();
    }
    variants
}

fn scan() -> HashSet<String> {
    #[cfg(windows)]
    {
        scan_windows()
    }
    #[cfg(not(windows))]
    {
        let mut found = scan_fc_list();
        if found.is_empty() {
            let home = std::env::var_os("HOME").map(PathBuf::from);
            let mut dirs: Vec<PathBuf> = ["/usr/share/fonts", "/usr/local/share/fonts", "/System/Library/Fonts", "/Library/Fonts"]
                .iter()
                .map(PathBuf::from)
                .collect();
            if let Some(home) = home {
                dirs.extend([home.join(".fonts"), home.join(".local/share/fonts"), home.join("Library/Fonts")]);
            }
            found = scan_dirs(&dirs);
        }
        found
    }
}

/// Nombres del registro: "Arial Black (TrueType)" y las variantes van en el
/// mismo valor.
#[cfg(windows)]
fn scan_windows() -> HashSet<String> {
    use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
    use winreg::RegKey;

    let mut found = HashSet::new();
    for hive in [HKEY_LOCAL_MACHINE, HKEY_CURRENT_USER] {
        let Ok(key) = RegKey::predef(hive).open_subkey(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion\Fonts") else {
            continue;
        };
        for (name, _) in key.enum_values().flatten() {
            // "Arial Black (TrueType)" -> "Arial Black"; ademas cada estilo
            // ("Arial Black Bold") se guarda aparte, asi que se anade tambien
            // la familia sin el estilo final.
            let clean = name.split('(').next().unwrap_or("").trim().to_string();
            for variant in family_variants(&clean) {
                found.insert(normalise(&variant));
            }
        }
    }
    found
}

#[cfg(not(windows))]
fn scan_fc_list() -> HashSet<String> {
    let Ok(output) = crate::ffmpeg::command("fc-list").args(["--format", "%{family}\n"]).output() else {
        return HashSet::new();
    };
    if !output.status.success() {
        return HashSet::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .flat_map(|line| line.split(',').map(|f| normalise(f.trim())).collect::<Vec<_>>())
        .filter(|f| !f.is_empty())
        .collect()
}

#[cfg(not(windows))]
fn scan_dirs(folders: &[PathBuf]) -> HashSet<String> {
    let mut found = HashSet::new();
    let mut pending: Vec<PathBuf> = folders.to_vec();
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|e| ["ttf", "otf", "ttc"].contains(&e.to_string_lossy().to_lowercase().as_str())) {
                found.insert(normalise(&path.file_stem().unwrap_or_default().to_string_lossy()));
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn familias(nombres: &[&str]) -> HashSet<String> {
        nombres.iter().map(|n| normalise(n)).collect()
    }

    #[test]
    fn las_variantes_van_de_mas_a_menos() {
        assert_eq!(family_variants("Arial Black Bold"), ["Arial Black Bold", "Arial Black", "Arial"]);
        assert!(family_variants("").is_empty());
    }

    #[test]
    fn una_fuente_inventada_no_esta_y_una_instalada_si() {
        let f = familias(&["Arial", "Arial Black", "Verdana Bold"]);
        assert!(!is_available_in("Fuente Que No Existe 12345", None, &f));
        assert!(is_available_in("arial  black", None, &f));
        assert!(is_available_in("Verdana", None, &f), "la familia empieza por el nombre pedido");
    }

    #[test]
    fn si_no_se_sabe_listar_las_fuentes_no_se_alarma() {
        assert!(is_available_in("Cualquiera", None, &HashSet::new()));
        assert!(is_available_in("", None, &familias(&["Arial"])));
    }

    #[test]
    fn una_carpeta_de_ttf_propios_vale() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("MiFuenteRara.ttf"), b"x").unwrap();
        std::fs::write(dir.path().join("notas.txt"), b"x").unwrap();
        let carpeta = dir.path().to_str().unwrap();
        assert!(is_available_in("MiFuenteRara", Some(carpeta), &familias(&["Arial"])));
        assert!(!is_available_in("notas", Some(carpeta), &familias(&["Arial"])));
    }

    #[test]
    fn la_alternativa_es_la_primera_instalada_de_la_lista() {
        assert_eq!(suggest_in(&familias(&["Impact", "Arial"])), "Impact");
        assert_eq!(suggest_in(&familias(&["Comic Sans"])), "Arial");
    }

    #[test]
    fn el_sistema_real_se_puede_listar_sin_romper() {
        let _ = installed_families();
    }
}
