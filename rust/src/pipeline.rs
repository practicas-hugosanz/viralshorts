//! El pipeline completo: de un podcast de YouTube a shorts verticales.
//!
//! Uso tipico:
//!
//! ```text
//! viralshorts --podcast <URL> --background <URL>
//! viralshorts --podcast <URL> --background gameplay.mp4 --clips 5
//! viralshorts --podcast <URL> --background <URL> --dry-run
//! ```
//!
//! Trabajo en dos tiempos (elegir ahora, montar despues de revisar):
//!
//! ```text
//! viralshorts --podcast <URL> --background <URL> --plan-out work/plans/plan.json
//! viralshorts --from-plan work/plans/plan.json --only 1,4,7
//! ```
//!
//! Todo lo ajustable esta en config.yaml. Los argumentos de linea de comandos
//! tienen prioridad, y `--set clave=valor` llega a cualquier ajuste.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use serde_json::{json, Value};

use crate::audio_energy::{self, EnergyTrack};
use crate::config::{Config, ConfigError};
use crate::download::{self, SourceVideo};
use crate::ffmpeg::{self, FfmpegError};
use crate::llm::{ClipJudge, ClipMetadata};
use crate::plan::{self, PlanSource};
use crate::progress;
use crate::render::{self, RenderRequest};
use crate::scoring::{self, hms, Window};
use crate::transcribe::{Transcriber, WhisperCpp};
use crate::worklock::WorkLock;
use crate::{context, exports, facecrop, fonts, jumpcuts, metadata, prescan, subtitles};

// ----------------------------------------------------------------------------
// Linea de comandos
// ----------------------------------------------------------------------------

#[derive(Debug, Clone, Default, clap::Args)]
pub struct Args {
    /// URL de YouTube o ruta a un fichero local.
    #[arg(long)]
    pub podcast: Option<String>,
    /// Video de fondo (gameplay). URL o fichero local.
    #[arg(long)]
    pub background: Option<String>,
    /// Fichero YAML de configuracion.
    #[arg(long, default_value = "config.yaml", global = true)]
    pub config: PathBuf,
    /// Cambia cualquier ajuste sin tocar el YAML. Ej: --set subtitles.font_size=80
    #[arg(long = "set", value_name = "CLAVE=VALOR", global = true)]
    pub overrides: Vec<String>,
    /// Cuantos shorts generar.
    #[arg(long, value_name = "N")]
    pub clips: Option<i64>,
    #[arg(long, value_name = "SEG")]
    pub min_duration: Option<f64>,
    #[arg(long, value_name = "SEG")]
    pub max_duration: Option<f64>,
    /// Segundos iniciales que se ignoran (intro, patrocinios).
    #[arg(long, value_name = "SEG")]
    pub head_skip: Option<f64>,
    /// Segundos finales que se ignoran (despedida).
    #[arg(long, value_name = "SEG")]
    pub tail_skip: Option<f64>,
    /// Carpeta de salida.
    #[arg(long, value_name = "DIR")]
    pub output: Option<String>,
    /// Modelo de Whisper: tiny, base, small, medium, large-v3.
    #[arg(long, value_name = "TAMANO")]
    pub model: Option<String>,
    /// Idioma del podcast (es, en...). 'auto' para detectarlo.
    #[arg(long, value_name = "COD")]
    pub language: Option<String>,
    /// Transcribir el video entero en vez de solo las zonas calientes.
    #[arg(long)]
    pub full_transcript: bool,
    /// Zonas a preseleccionar y transcribir (por defecto: auto).
    #[arg(long, value_name = "N")]
    pub regions: Option<i64>,
    /// beam_size de Whisper. 1 = greedy (rapido), 5 = mejor calidad.
    #[arg(long, value_name = "N")]
    pub beam: Option<i64>,
    /// No usar Claude: puntuar solo con heatmap, energia y densidad.
    #[arg(long)]
    pub no_llm: bool,
    /// No tapar las palabrotas en los subtitulos.
    #[arg(long)]
    pub no_censor: bool,
    /// No mostrar la tarjeta de contexto del principio.
    #[arg(long)]
    pub no_title_card: bool,
    /// No tener en cuenta si el clip se entiende sin contexto.
    #[arg(long)]
    pub no_context: bool,
    /// No escribir titulo, gancho ni hashtags.
    #[arg(long)]
    pub no_metadata: bool,
    /// Desactivar el seguimiento de caras (recorte centrado).
    #[arg(long)]
    pub no_reframe: bool,
    /// No eliminar silencios.
    #[arg(long)]
    pub no_jumpcuts: bool,
    /// Desactivar el punch-in (render mas rapido).
    #[arg(long)]
    pub no_zoom: bool,
    /// Elegir y mostrar los clips sin renderizar nada.
    #[arg(long)]
    pub dry_run: bool,
    /// Elegir los momentos y guardarlos para revisarlos, sin renderizar.
    #[arg(long, value_name = "FICHERO", help_heading = "Montaje en dos tiempos")]
    pub plan_out: Option<PathBuf>,
    /// Cuantos candidatos proponer en el plan (por defecto: el triple de los pedidos).
    #[arg(long, value_name = "N", help_heading = "Montaje en dos tiempos")]
    pub propose: Option<i64>,
    /// Renderizar los clips marcados en un plan guardado.
    #[arg(long, value_name = "FICHERO", help_heading = "Montaje en dos tiempos")]
    pub from_plan: Option<PathBuf>,
    /// Con --from-plan: renderizar solo estos numeros de clip.
    #[arg(long, value_name = "N,N", help_heading = "Montaje en dos tiempos")]
    pub only: Option<String>,
    #[arg(short, long, global = true)]
    pub verbose: bool,
}

pub fn apply_overrides(cfg: &mut Config, args: &Args) -> Result<(), ConfigError> {
    if let Some(n) = args.clips {
        cfg.clips.num_clips = n;
    }
    if let Some(v) = args.min_duration {
        cfg.clips.min_duration = v;
    }
    if let Some(v) = args.max_duration {
        cfg.clips.max_duration = v;
    }
    if let Some(v) = args.head_skip {
        cfg.clips.head_skip = v;
    }
    if let Some(v) = args.tail_skip {
        cfg.clips.tail_skip = v;
    }
    if let Some(dir) = &args.output {
        cfg.output.dir = dir.clone();
    }
    if let Some(model) = &args.model {
        cfg.transcribe.model_size = model.clone();
    }
    if let Some(language) = &args.language {
        cfg.transcribe.language = if language == "auto" { None } else { Some(language.clone()) };
    }
    if args.full_transcript {
        cfg.prescan.enabled = false;
    }
    if let Some(n) = args.regions {
        cfg.prescan.regions = n;
    }
    if let Some(n) = args.beam {
        cfg.transcribe.beam_size = n;
    }
    if args.no_llm {
        cfg.llm.enabled = false;
    }
    if args.no_metadata {
        cfg.metadata.enabled = false;
    }
    if args.no_context {
        cfg.context.enabled = false;
    }
    if args.no_title_card {
        cfg.title_card.enabled = false;
    }
    if args.no_censor {
        cfg.subtitles.censor_profanity = false;
    }
    if args.no_reframe {
        cfg.reframe.enabled = false;
    }
    if args.no_jumpcuts {
        cfg.jumpcuts.enabled = false;
    }
    if args.no_zoom {
        cfg.zoom.enabled = false;
    }

    for item in &args.overrides {
        cfg.set(item)?;
    }

    // Con el podcast a pantalla completa o en una franja no hay costura donde
    // poner la tarjeta de contexto: va arriba.
    if cfg.layout.mode != "stack" && cfg.title_card.anchor == "center" {
        cfg.title_card.anchor = "top".into();
    }
    if !["stack", "full", "center"].contains(&cfg.layout.mode.as_str()) {
        return Err(ConfigError::Invalid(format!("layout.mode '{}' no existe: usa stack, full o center.", cfg.layout.mode)));
    }
    if !["classic", "box", "marker", "neon", "minimal"].contains(&cfg.subtitles.style.as_str()) {
        return Err(ConfigError::Invalid(format!("subtitles.style '{}' no existe: usa classic, box, marker, neon o minimal.", cfg.subtitles.style)));
    }

    if cfg.clips.min_duration > cfg.clips.max_duration {
        return Err(ConfigError::Invalid("clips.min_duration no puede ser mayor que clips.max_duration.".into()));
    }
    Ok(())
}

// ----------------------------------------------------------------------------
// Entrada
// ----------------------------------------------------------------------------

/// Ejecuta el pipeline y devuelve el codigo de salida (0 bien, 1 fallo del
/// trabajo, 2 mal uso o mala configuracion).
pub fn run(args: &Args) -> i32 {
    if args.from_plan.is_none() && !(args.podcast.is_some() && args.background.is_some()) {
        log::error!("Hacen falta --podcast y --background (o --from-plan).");
        return 2;
    }

    let mut cfg = match load_config(args) {
        Ok(cfg) => cfg,
        Err(error) => {
            log::error!("Configuracion invalida: {error}");
            return 2;
        }
    };

    if let Err(error) = ffmpeg::require_ffmpeg() {
        log::error!("{error}");
        return 2;
    }

    let started = Instant::now();
    let work_dir = PathBuf::from(&cfg.paths.work_dir);
    let output_dir = PathBuf::from(&cfg.output.dir);
    if let Err(error) = std::fs::create_dir_all(&output_dir) {
        log::error!("No se pudo crear la carpeta de salida {}: {error}", output_dir.display());
        return 2;
    }

    // El cerrojo cubre toda la ejecucion: descargas, cache y ficheros
    // temporales del render viven en work_dir y no admiten dos duenos.
    let mut lock = WorkLock::new(&work_dir);
    if let Err(error) = lock.acquire() {
        log::error!("{error}");
        return 2;
    }

    let code = match &args.from_plan {
        Some(path) => run_from_plan(args, path, &mut cfg, &work_dir, started),
        None => run_new(args, &cfg, &work_dir, &output_dir, started),
    };
    lock.release();
    code
}

/// `config.yaml` de la carpeta actual o, si no hay, el de la carpeta de datos
/// del programa (donde lo deja la instalacion).
pub fn resolve_config(path: &Path) -> PathBuf {
    if path.is_file() || path.is_absolute() {
        return path.to_path_buf();
    }
    let in_home = ffmpeg::project_root().join(path);
    if in_home.is_file() { in_home } else { path.to_path_buf() }
}

fn load_config(args: &Args) -> Result<Config, ConfigError> {
    let path = resolve_config(&args.config);
    let mut cfg = Config::load(path.is_file().then_some(path.as_path()))?;
    apply_overrides(&mut cfg, args)?;
    Ok(cfg)
}

/// Avisa ANTES de renderizar si la fuente de los subtitulos no existe. libass
/// no protesta: cuando no encuentra la fuente pone otra y sigue, y solo se
/// descubre al ver el short.
fn check_fonts(cfg: &Config) {
    if !cfg.subtitles.enabled {
        return;
    }
    let title_font = if cfg.title_card.font_name.is_empty() { &cfg.subtitles.font_name } else { &cfg.title_card.font_name };
    for (label, name) in [("subtitulos", &cfg.subtitles.font_name), ("gancho", &cfg.hook.font_name), ("tarjeta", title_font)] {
        if name.is_empty() || fonts::is_available(name, cfg.subtitles.fonts_dir.as_deref()) {
            continue;
        }
        let alternative = fonts::suggest_alternative(name);
        log::warn!(
            "La fuente '{name}' ({label}) no esta instalada en el sistema; ffmpeg pondra otra. Instalala, o usa: --set subtitles.font_name=\"{alternative}\""
        );
        progress::result(
            "aviso",
            json!({
                "titulo": format!("Falta la fuente «{name}»"),
                "texto": format!(
                    "Los subtitulos saldran con otra fuente cualquiera. Instalala en el sistema (no basta con tener el .ttf suelto) o elige «{alternative}»."
                ),
            }),
        );
        break;
    }
}

// ----------------------------------------------------------------------------
// Fuentes de video
// ----------------------------------------------------------------------------

/// Lo poco que el render necesita de un video ya descargado.
#[derive(Debug, Clone)]
pub struct Source {
    pub path: PathBuf,
    pub title: String,
    pub duration: f64,
    pub url: String,
}

impl From<&SourceVideo> for Source {
    fn from(video: &SourceVideo) -> Self {
        Self { path: video.path.clone(), title: video.title.clone(), duration: video.duration, url: video.webpage_url().to_string() }
    }
}

impl From<&PlanSource> for Source {
    fn from(plan: &PlanSource) -> Self {
        Self { path: PathBuf::from(&plan.ruta), title: plan.titulo.clone(), duration: plan.duracion, url: plan.url.clone() }
    }
}

// ----------------------------------------------------------------------------
// Camino normal: elegir y (si toca) montar
// ----------------------------------------------------------------------------

struct Selection {
    clips: Vec<Window>,
    podcast: SourceVideo,
    background: SourceVideo,
    judge: ClipJudge,
    energy: EnergyTrack,
}

fn run_new(args: &Args, cfg: &Config, work_dir: &Path, output_dir: &Path, started: Instant) -> i32 {
    check_fonts(cfg);

    let making_plan = args.plan_out.is_some();
    let proposed = if making_plan { proposed_count(args, cfg) } else { cfg.clips.num_clips.max(0) as usize };

    let mut selection = match select_clips(args, cfg, work_dir, Some(proposed)) {
        Ok(selection) => selection,
        Err(error) => {
            log::error!("No se pudieron seleccionar los clips: {error}");
            return 1;
        }
    };

    report_selection(&selection.clips);
    progress::result(
        "clips",
        json!({"clips": selection.clips.iter().enumerate().map(|(i, c)| json!({
            "n": i + 1,
            "inicio": round(c.start, 2),
            "fin": round(c.end, 2),
            "marca": hms(c.start),
            "duracion": round(c.duration(), 1),
            "puntuacion": round(c.score, 3),
            "motivo": c.reason,
            "texto": c.text.chars().take(400).collect::<String>(),
        })).collect::<Vec<_>>()}),
    );

    if args.dry_run && !making_plan {
        log::info!("--dry-run: no se renderiza nada.");
        return 0;
    }

    let podcast = Source::from(&selection.podcast);
    let background = Source::from(&selection.background);

    if let Some(plan_path) = &args.plan_out {
        // Los textos solo para los que vienen marcados: los demas se escriben si
        // el usuario acaba eligiendolos, y no antes.
        let preselected = (cfg.clips.num_clips.max(0) as usize).min(selection.clips.len());
        progress::stage("metadatos", "Escribiendo titulos y hashtags");
        let written = metadata_for(&mut selection.judge, &selection.clips[..preselected], &podcast.title, cfg);
        let saved = plan::save(
            plan_path,
            cfg,
            &PlanSource::from_video(&selection.podcast),
            &PlanSource::from_video(&selection.background),
            &selection.clips,
            &written,
            preselected,
        );
        return match saved.and_then(|path| plan::load(&path).map(|loaded| (path, loaded))) {
            Ok((path, loaded)) => {
                progress::result(
                    "plan",
                    json!({"ruta": std::path::absolute(&path).unwrap_or(path.clone()).to_string_lossy(), "clips": plan_events(&loaded)}),
                );
                log::info!("Plan listo: {}. Revisalo y monta con --from-plan.", path.display());
                0
            }
            Err(error) => {
                log::error!("No se pudo guardar el plan: {error}");
                1
            }
        };
    }

    progress::stage("metadatos", "Escribiendo titulos y hashtags");
    let written = metadata_for(&mut selection.judge, &selection.clips, &podcast.title, cfg);
    let output_dir = run_output_dir(cfg, output_dir, &podcast.title, &chrono::Local::now().format("%Y%m%d_%H%M%S").to_string());
    let numbers: Vec<usize> = (1..=selection.clips.len()).collect();
    let results = render_all(&selection.clips, &written, &podcast, &background, cfg, work_dir, &output_dir, &numbers, Some(&selection.energy));

    finish(cfg, &output_dir);
    report_results(&results, selection.clips.len(), started.elapsed().as_secs_f64())
}

/// Carpeta donde caen los shorts de ESTA ejecucion, y se la dice a la interfaz.
pub fn run_output_dir(cfg: &Config, base: &Path, title: &str, tag: &str) -> PathBuf {
    let folder = if cfg.output.run_subfolder { exports::run_folder(base, title, tag) } else { base.to_path_buf() };
    let _ = std::fs::create_dir_all(&folder);
    progress::result("salida", json!({"ruta": std::path::absolute(&folder).unwrap_or(folder.clone()).to_string_lossy()}));
    folder
}

/// Cuantos candidatos se proponen para revisar a mano: el triple de los
/// pedidos, suficiente para descartar los que no valen sin convertir la
/// revision en un trabajo de media hora.
pub fn proposed_count(args: &Args, cfg: &Config) -> usize {
    let wanted = cfg.clips.num_clips.max(0) as usize;
    match args.propose.filter(|n| *n > 0) {
        Some(n) => wanted.max(n as usize),
        None => (wanted * 3).max(6).min(20),
    }
}

/// Descarga, transcribe, puntua y devuelve los mejores clips.
fn select_clips(args: &Args, cfg: &Config, work_dir: &Path, limit: Option<usize>) -> Result<Selection, String> {
    progress::stage("descarga", "Descargando los videos");
    let podcast = download::fetch(args.podcast.as_deref().unwrap_or(""), &work_dir.join("podcast"), &cfg.download).map_err(|e| e.to_string())?;
    let background = download::fetch(args.background.as_deref().unwrap_or(""), &work_dir.join("background"), &cfg.download).map_err(|e| e.to_string())?;

    // El orden importa: la energia y el heatmap no necesitan texto y se calculan
    // en segundos, asi que sirven para decidir QUE hay que transcribir.
    progress::stage("analisis", "Analizando audio y buscando momentos");
    let energy = audio_energy::analyse(&podcast.path, podcast.duration).map_err(|e| e.to_string())?;

    let regions = cfg.prescan.enabled.then(|| prescan::choose_regions(podcast.duration, &podcast.heatmap, Some(&energy), &cfg.prescan, &cfg.clips, &cfg.scoring));

    progress::stage("transcripcion", "Transcribiendo el podcast");
    let engine = WhisperCpp::discover(&ffmpeg::project_root()).map_err(|e| e.to_string())?;
    let mut transcriber = Transcriber::new(cfg.transcribe.clone(), &cfg.paths.cache_dir, Box::new(engine)).map_err(|e| e.to_string())?;
    let transcript = match &regions {
        Some(regions) if !regions.is_empty() => transcriber.transcribe_regions(&podcast.path, podcast.duration, regions),
        _ => transcriber.transcribe(&podcast.path, podcast.duration),
    }
    .map_err(|e| e.to_string())?;

    let mut windows = scoring::build_windows(&transcript, &cfg.clips, regions.as_deref()).map_err(|e| e.to_string())?;
    scoring::apply_cheap_signals(&mut windows, &podcast.heatmap, Some(&energy));

    if cfg.context.enabled {
        // El heatmap dice donde esta lo bueno; esto dice donde EMPIEZA, para que
        // el clip no arranque a mitad de una conversacion que nadie ha visto.
        let topics = context::build_topic_map(&transcript, &cfg.context);
        context::apply(&mut windows, &topics, &cfg.context);
    }

    let mut judge = ClipJudge::new(cfg.llm.clone(), Some(Path::new(&cfg.paths.cache_dir)));
    let mut clips;
    if cfg.llm.enabled {
        let mut shortlist = scoring::shortlist_for_llm(&windows, cfg.scoring.llm_candidates.max(0) as usize, &cfg.scoring);
        progress::stage("claude", "Claude esta puntuando los fragmentos");
        let llm_scores = judge.score_windows(&shortlist);
        scoring::apply_llm_scores(&mut shortlist, &llm_scores);
        // Solo compiten las ventanas que Claude ha visto: mezclarlas con las
        // miles que no ha puntuado haria ganar siempre a una sin puntuar.
        clips = if llm_scores.is_empty() { windows } else { shortlist };
    } else {
        log::info!("Claude desactivado (--no-llm): puntuando solo con heuristicas.");
        clips = windows;
    }

    let mut picked = scoring::finalise(&mut clips, &cfg.scoring, &cfg.clips, limit);
    if picked.is_empty() {
        return Err("No se selecciono ningun clip.".into());
    }
    refine_clips(cfg, &podcast.path, podcast.duration, &mut picked);
    Ok(Selection { clips: picked, podcast, background, judge, energy })
}

/// Vuelve a transcribir SOLO el trozo de cada clip elegido con un modelo mejor.
/// La pasada larga (todo el podcast) tiene que ser rapida para poder elegir; el
/// texto que acaba en pantalla son unas decenas de segundos por clip, asi que
/// ahi si compensa gastar el modelo grande. Si algo falla se queda lo de antes.
fn refine_clips(cfg: &Config, podcast: &Path, duration: f64, clips: &mut [Window]) {
    let model = cfg.transcribe.refine_model.trim();
    if model.is_empty() || clips.is_empty() {
        return;
    }
    progress::stage("transcripcion", "Afinando el texto de los clips elegidos");
    log::info!("Afinando el texto de {} clips con el modelo '{model}' (solo sus segundos, no el pódcast entero).", clips.len());

    let outcome = (|| -> Result<Vec<Option<(Vec<crate::words::Word>, String)>>, String> {
        let mut refine = cfg.transcribe.clone();
        refine.model_size = model.to_string();
        refine.beam_size = refine.beam_size.max(5);
        let engine = WhisperCpp::discover(&ffmpeg::project_root()).map_err(|e| e.to_string())?;
        let mut transcriber = Transcriber::new(refine, &cfg.paths.cache_dir, Box::new(engine)).map_err(|e| e.to_string())?;
        let regions: Vec<(f64, f64)> = clips.iter().map(|c| ((c.start - 1.0).max(0.0), (c.end + 1.0).min(duration))).collect();
        let transcript = transcriber.transcribe_regions(podcast, duration, &regions).map_err(|e| e.to_string())?;
        let all: Vec<&crate::words::Word> = transcript.segments.iter().flat_map(|s| s.words.iter()).collect();
        Ok(clips
            .iter()
            .map(|clip| {
                let mut words: Vec<crate::words::Word> =
                    all.iter().filter(|w| w.start >= clip.start - 0.25 && w.end <= clip.end + 0.25).map(|w| (*w).clone()).collect();
                words.sort_by(|a, b| a.start.total_cmp(&b.start));
                words.dedup_by(|a, b| (a.start - b.start).abs() < 0.05 && a.text == b.text);
                // Una pasada que devuelve la mitad de las palabras ha ido mal.
                (words.len() >= 3 && words.len() * 2 >= clip.words.len()).then(|| {
                    let text = words.iter().map(|w| w.text.as_str()).collect::<Vec<_>>().join(" ");
                    (words, text)
                })
            })
            .collect())
    })();

    match outcome {
        Ok(results) => {
            let mut refined = 0;
            for (clip, result) in clips.iter_mut().zip(results) {
                match result {
                    Some((words, text)) => {
                        clip.words = words;
                        clip.text = text;
                        refined += 1;
                    }
                    None => log::warn!("El clip de {} no se pudo afinar; se queda el texto de la primera pasada.", hms(clip.start)),
                }
            }
            log::info!("Texto afinado en {refined} de {} clips.", clips.len());
        }
        Err(error) => log::warn!("No se pudo afinar el texto ({error}). Se sigue con el de la primera pasada."),
    }
}

// ----------------------------------------------------------------------------
// Render a partir de un plan revisado
// ----------------------------------------------------------------------------

fn run_from_plan(args: &Args, plan_path: &Path, cfg: &mut Config, work_dir: &Path, started: Instant) -> i32 {
    let saved = match plan::load(plan_path) {
        Ok(saved) => saved,
        Err(error) => {
            log::error!("{error}");
            return 2;
        }
    };

    // La configuracion del plan manda: es con la que se eligieron los clips.
    // Encima se aplican los argumentos de esta llamada (carpeta de salida, --set
    // de estilo), que es lo que el usuario acaba de pedir.
    let rebuilt = (|| {
        let mut fresh = Config::default();
        fresh.merge(&saved.config, "plan")?;
        apply_overrides(&mut fresh, args)?;
        Ok::<_, ConfigError>(fresh)
    })();
    match rebuilt {
        Ok(fresh) => *cfg = fresh,
        Err(error) => {
            log::error!("El plan trae ajustes que esta version no entiende: {error}");
            return 2;
        }
    }

    // La carpeta de salida sale de la configuracion ya fusionada: el plan trae
    // la suya, y --output de esta llamada manda sobre ella.
    let output_dir = PathBuf::from(&cfg.output.dir);
    let _ = std::fs::create_dir_all(&output_dir);
    check_fonts(cfg);

    let wanted = wanted_numbers(args, &saved);
    if wanted.is_empty() {
        log::error!("No hay ningun clip marcado para montar en {}.", plan_path.file_name().unwrap_or_default().to_string_lossy());
        return 2;
    }

    let podcast = Source::from(&saved.podcast);
    let background = Source::from(&saved.background);
    for (source, label) in [(&podcast, "podcast"), (&background, "video de fondo")] {
        if !source.path.is_file() {
            log::error!("El {label} del plan ya no esta en {}. Vuelve a buscar los momentos para descargarlo otra vez.", source.path.display());
            return 2;
        }
    }

    // Misma carpeta cada vez que se monta el mismo plan: volver a montarlo tras
    // corregir un titulo reemplaza sus shorts, no los duplica.
    let stem = plan_path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
    let output_dir = run_output_dir(cfg, &output_dir, &podcast.title, stem.strip_prefix("plan_").unwrap_or(&stem));

    let clips: Vec<Window> = wanted.iter().map(|&n| saved.clips[n - 1].to_window()).collect();
    let mut written: HashMap<usize, ClipMetadata> =
        wanted.iter().enumerate().filter_map(|(index, &n)| saved.clips[n - 1].meta.clone().map(|m| (index, m))).collect();

    let missing: Vec<usize> = (0..clips.len()).filter(|i| !written.contains_key(i)).collect();
    if !missing.is_empty() {
        // Clips que el usuario ha marcado despues: no tenian textos escritos.
        progress::stage("metadatos", "Escribiendo titulos y hashtags");
        let mut judge = ClipJudge::new(cfg.llm.clone(), Some(Path::new(&cfg.paths.cache_dir)));
        let extra_clips: Vec<Window> = missing.iter().map(|&i| clips[i].clone()).collect();
        let extra = metadata_for(&mut judge, &extra_clips, &podcast.title, cfg);
        for (position, &index) in missing.iter().enumerate() {
            if let Some(meta) = extra.get(&position) {
                written.insert(index, meta.clone());
            }
        }
    }

    log::info!(
        "Montando {} clip(s) del plan {}: {}",
        clips.len(),
        plan_path.file_name().unwrap_or_default().to_string_lossy(),
        wanted.iter().map(|n| n.to_string()).collect::<Vec<_>>().join(", ")
    );
    let results = render_all(&clips, &written, &podcast, &background, cfg, work_dir, &output_dir, &wanted, None);

    finish(cfg, &output_dir);
    report_results(&results, clips.len(), started.elapsed().as_secs_f64())
}

/// Numeros de clip (desde 1) a montar: los de --only, o los marcados en el plan.
pub fn wanted_numbers(args: &Args, saved: &plan::Plan) -> Vec<usize> {
    let Some(only) = &args.only else { return saved.selected().into_iter().map(|i| i + 1).collect() };

    let mut wanted: Vec<usize> = Vec::new();
    for chunk in only.replace(' ', "").split(',').filter(|c| !c.is_empty()) {
        match chunk.parse::<usize>() {
            Ok(n) if (1..=saved.clips.len()).contains(&n) => wanted.push(n),
            Ok(n) => log::warn!("--only: el plan no tiene ningun clip {n}."),
            Err(_) => log::warn!("--only: '{chunk}' no es un numero de clip."),
        }
    }
    wanted.sort_unstable();
    wanted.dedup();
    wanted
}

/// Los candidatos del plan, en el formato que consume la interfaz.
pub fn plan_events(saved: &plan::Plan) -> Vec<Value> {
    saved
        .clips
        .iter()
        .enumerate()
        .map(|(index, clip)| {
            let meta = clip.meta.as_ref();
            json!({
                "n": index + 1,
                "inicio": round(clip.start, 2),
                "fin": round(clip.end, 2),
                "marca": hms(clip.start),
                "duracion": round(clip.duration(), 1),
                "puntuacion": round(clip.score, 3),
                "motivo": clip.reason,
                "texto": clip.text,
                "seleccionado": clip.selected,
                "titulo": meta.map_or("", |m| m.title.as_str()),
                "gancho": meta.map_or("", |m| m.hook.as_str()),
                "descripcion": meta.map_or("", |m| m.description.as_str()),
                "hashtags": meta.map_or(Vec::new(), |m| m.hashtags.clone()),
            })
        })
        .collect()
}

// ----------------------------------------------------------------------------
// Textos de publicacion
// ----------------------------------------------------------------------------

/// Textos de publicacion: Claude si esta disponible, si no por extraccion.
///
/// Los dos caminos se combinan a proposito. Cuando Claude contesta a medias --
/// se deja un clip, o devuelve el titulo pero no los hashtags -- lo que falta se
/// rellena con el metodo heuristico en vez de salir vacio.
pub fn metadata_for(judge: &mut ClipJudge, clips: &[Window], title: &str, cfg: &Config) -> HashMap<usize, ClipMetadata> {
    if clips.is_empty() {
        return HashMap::new();
    }

    let mut written = HashMap::new();
    if cfg.llm.enabled {
        written = judge.write_metadata(clips, title);
    }
    if !cfg.metadata.enabled {
        return written;
    }

    let complete = written.values().filter(|m| !m.title.is_empty() && !m.hook.is_empty() && !m.hashtags.is_empty()).count();
    if complete < clips.len() {
        if cfg.llm.enabled {
            log::info!("Completando los textos de {} clips sin modelo de lenguaje.", clips.len() - complete);
        } else {
            log::info!("Escribiendo titulos y hashtags por extraccion (sin Claude).");
        }
        written = metadata::write_metadata(clips, title, cfg, Some(&written));
    }
    written
}

// ----------------------------------------------------------------------------
// Render
// ----------------------------------------------------------------------------

/// Cuantos shorts montar a la vez. ffmpeg reparte el filtrado y la codificacion
/// entre varios hilos, pero hay tramos serie (el reencuadre, el arranque, la
/// escritura) en los que la maquina se queda a medio gas. Con dos renders a la
/// vez esos huecos los llena el otro clip.
pub fn render_workers(cfg: &Config, total: usize) -> usize {
    let configured = cfg.output.render_workers.max(0) as usize;
    if configured > 0 {
        return configured.min(total).max(1);
    }
    let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
    let auto = if cores >= 8 { 2 } else { 1 };
    auto.min(total).max(1)
}

type RenderResult = (usize, Result<PathBuf, String>);

#[allow(clippy::too_many_arguments)]
fn render_all(
    clips: &[Window],
    written: &HashMap<usize, ClipMetadata>,
    podcast: &Source,
    background: &Source,
    cfg: &Config,
    work_dir: &Path,
    output_dir: &Path,
    numbers: &[usize],
    energy: Option<&EnergyTrack>,
) -> Vec<RenderResult> {
    let background_duration = ffmpeg::probe(&background.path).map(|i| i.duration).unwrap_or(background.duration);
    let workers = render_workers(cfg, clips.len());

    if workers > 1 {
        // Con varios ffmpeg a la vez, el porcentaje de cada uno pisa al de los
        // demas; se informa del avance por clips terminados.
        progress::suppress("render");
        log::info!("Montando {} shorts, {workers} a la vez.", clips.len());
        progress::stage("render", &format!("Montando {} shorts ({workers} a la vez)", clips.len()));
    }

    let next = AtomicUsize::new(0);
    let done = AtomicUsize::new(0);
    let results: Mutex<Vec<RenderResult>> = Mutex::new(Vec::new());

    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| loop {
                let index = next.fetch_add(1, Ordering::SeqCst);
                if index >= clips.len() {
                    break;
                }
                let (clip, number, meta) = (&clips[index], numbers[index], written.get(&index));
                if workers == 1 {
                    progress::stage("render", &format!("Renderizando clip {number} de {}", clips.len()));
                    log::info!("{}", "=".repeat(68));
                    log::info!("CLIP {number}/{}  {}", clips.len(), clip.describe());
                    if !clip.reason.is_empty() {
                        log::info!("Motivo: {}", clip.reason);
                    }
                }

                let outcome = render_one(clip, number, meta, podcast, background, background_duration, cfg, work_dir, output_dir, energy);
                match &outcome {
                    Ok(path) => progress::result(
                        "clip_done",
                        json!({
                            "n": number,
                            "ruta": std::path::absolute(path).unwrap_or(path.clone()).to_string_lossy(),
                            "fichero": path.file_name().unwrap_or_default().to_string_lossy(),
                            "titulo": meta.map_or("", |m| m.title.as_str()),
                            "gancho": meta.map_or("", |m| m.hook.as_str()),
                            "pie": meta.map_or(String::new(), |m| m.caption()),
                        }),
                    ),
                    // Un clip que falla no debe tirar los demas.
                    Err(error) => {
                        log::error!("El clip {number} fallo: {error}");
                        progress::result("clip_failed", json!({"n": number, "error": error}));
                    }
                }
                results.lock().unwrap().push((number, outcome));

                let finished = done.fetch_add(1, Ordering::SeqCst) + 1;
                if workers > 1 {
                    progress::progress("render_total", finished as f64 / clips.len() as f64, &format!("Montando shorts ({finished}/{})", clips.len()), finished == clips.len());
                }
            });
        }
    });

    progress::allow("render");
    let mut results = results.into_inner().unwrap_or_default();
    results.sort_by_key(|(number, _)| *number);
    results
}

#[allow(clippy::too_many_arguments)]
fn render_one(
    clip: &Window,
    number: usize,
    meta: Option<&ClipMetadata>,
    podcast: &Source,
    background: &Source,
    background_duration: f64,
    cfg: &Config,
    work_dir: &Path,
    output_dir: &Path,
    energy: Option<&EnergyTrack>,
) -> Result<PathBuf, String> {
    // 1. Palabras del clip, con el tiempo relativo a su inicio.
    let words: Vec<_> = clip.words.iter().map(|w| w.shifted(-clip.start)).collect();

    // 2. Que silencios se quitan, y como afecta eso a los tiempos.
    let cuts = jumpcuts::plan(&words, clip.duration(), &cfg.jumpcuts);
    let words = cuts.remap_words(&words);

    // 3. Trayectoria del encuadre (sobre el clip sin cortar). La curva de
    //    energia le dice cuando hay alguien hablando y cuando no; si no viene
    //    dada (montaje desde un plan) se calcula solo la de este clip, que son
    //    unas decimas de segundo.
    let own_energy;
    let energy = match energy {
        Some(e) => Some(e),
        None if cfg.reframe.enabled && cfg.reframe.speaker_tracking => {
            own_energy = audio_energy::analyse_range(&podcast.path, clip.start, clip.duration()).map_err(|e| log::debug!("Sin energia para el clip {number} ({e}).")).ok();
            own_energy.as_ref()
        }
        None => None,
    };
    let track = facecrop::track_faces(&podcast.path, clip.start, clip.duration(), &cfg.reframe, Some(Path::new(&cfg.paths.cache_dir)), energy);

    // 4. Subtitulos + rotulo de gancho.
    let mut ass_name = None;
    if cfg.subtitles.enabled || !cfg.brand.handle.trim().is_empty() {
        let title_text = metadata::card_text(&podcast.title, meta.map_or("", |m| m.title.as_str()), cfg);
        let mut content = subtitles::build_ass(
            &words,
            cfg.layout.width,
            cfg.layout.height,
            &cfg.subtitles,
            Some(&cfg.hook),
            meta.map(|m| m.hook.as_str()),
            cuts.output_duration(),
            Some(&cfg.title_card),
            Some(&title_text),
        )
        .map_err(|e| e.to_string())?;
        // El usuario del canal va junto al logo, asi que hace falta su tamano.
        let logo_size = (!cfg.brand.logo.is_empty())
            .then(|| ffmpeg::image_size(Path::new(&cfg.brand.logo)))
            .flatten()
            .map(|(logo_w, logo_h)| {
                let w = ffmpeg::even((cfg.layout.width as f64 * cfg.brand.scale.clamp(0.03, 0.6)).round() as i64);
                (w, (w as f64 * logo_h as f64 / logo_w as f64).round() as i64)
            });
        subtitles::append_handle(&mut content, &cfg.brand, cfg.layout.width, cfg.layout.height, cuts.output_duration(), &cfg.subtitles, logo_size);
        let name = format!("short_{number}.ass");
        subtitles::write_ass(&content, &work_dir.join(&name)).map_err(|e| format!("No se pudo escribir {name}: {e}"))?;
        ass_name = Some(name);
    }

    // 5. Punto de entrada del fondo, distinto para cada clip y con vuelta al
    //    principio si el gameplay se acaba.
    let usable = (background_duration - clip.duration()).max(1.0);
    let offset = ((number as f64 - 1.0) * clip.duration()) % usable;

    let output = output_dir.join(format!("short_{number}.mp4"));
    render::render(
        &RenderRequest {
            podcast: podcast.path.clone(),
            background: background.path.clone(),
            start: clip.start,
            end: clip.end,
            background_offset: offset,
            plan: cuts.clone(),
            track,
            ass_name,
            output: output.clone(),
            work_dir: work_dir.to_path_buf(),
        },
        cfg,
    )
    .map_err(|e: FfmpegError| e.to_string())?;

    if cfg.output.thumbnail {
        render::extract_thumbnail(&output, cfg.output.thumbnail_at);
    }

    write_sidecar(&output, clip, meta, podcast, &cuts);
    Ok(output)
}

/// Guarda los textos de publicacion junto al video.
fn write_sidecar(output: &Path, clip: &Window, meta: Option<&ClipMetadata>, podcast: &Source, cuts: &jumpcuts::JumpCutPlan) {
    let mut data = json!({
        "video": output.file_name().unwrap_or_default().to_string_lossy(),
        "fuente": {
            "titulo": podcast.title,
            "url": if podcast.url.is_empty() { Value::Null } else { Value::String(podcast.url.clone()) },
            "inicio": round(clip.start, 2),
            "fin": round(clip.end, 2),
            "marca": hms(clip.start),
        },
        "duracion_final": round(cuts.output_duration(), 2),
        "silencio_eliminado": round(cuts.removed(), 2),
        "puntuacion": {
            "total": round(clip.score, 4),
            "heatmap": round(clip.heat, 4),
            "energia": round(clip.energy, 4),
            "densidad": round(clip.density, 4),
            "contexto": round(clip.context, 4),
            "contenido_llm": round(clip.llm_content, 4),
            "gancho_llm": round(clip.llm_hook, 4),
            "motivo": clip.reason,
        },
        "transcripcion": clip.text,
    });
    if let Some(meta) = meta {
        data["publicacion"] = serde_json::to_value(meta).unwrap_or(Value::Null);
    }

    if let Err(error) = std::fs::write(output.with_extension("json"), serde_json::to_string_pretty(&data).unwrap_or_default()) {
        log::warn!("No se pudo escribir el .json de {}: {error}", output.display());
    }
    if let Some(meta) = meta {
        let _ = std::fs::write(output.with_extension("txt"), format!("{}\n\n{}\n", meta.title, meta.caption()));
    }
}

/// Lo que se hace una vez, cuando ya estan todos los shorts.
fn finish(cfg: &Config, output_dir: &Path) {
    if cfg.output.csv {
        exports::write_csv(output_dir, "publicaciones.csv");
    }
}

// ----------------------------------------------------------------------------
// Informes
// ----------------------------------------------------------------------------

fn report_selection(clips: &[Window]) {
    log::info!("{}", "=".repeat(68));
    log::info!("{} momentos seleccionados:", clips.len());
    for (index, clip) in clips.iter().enumerate() {
        log::info!("  {}. {}", index + 1, clip.describe());
        if !clip.reason.is_empty() {
            log::info!("     {}", clip.reason);
        }
        let preview: String = clip.text.chars().take(110).collect::<String>().replace('\n', " ");
        log::info!("     \"{preview}...\"");
    }
}

fn report_results(results: &[RenderResult], total: usize, elapsed: f64) -> i32 {
    let ok = results.iter().filter(|(_, r)| r.is_ok()).count();

    log::info!("{}", "=".repeat(68));
    log::info!("RESUMEN: {ok}/{total} shorts generados en {elapsed:.0}s");
    for (_, outcome) in results {
        if let Ok(path) = outcome {
            log::info!("  OK  {}", std::path::absolute(path).unwrap_or(path.clone()).display());
        }
    }
    for (number, outcome) in results {
        if let Err(error) = outcome {
            log::error!("  XX  clip {number}: {error}");
        }
    }

    progress::result("finished", json!({"ok": ok, "total": total}));
    if ok > 0 { 0 } else { 1 }
}

fn round(value: f64, places: i32) -> f64 {
    let factor = 10f64.powi(places);
    (value * factor).round_ties_even() / factor
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::words::Word;

    fn args(lista: &[&str]) -> Args {
        use clap::Parser;
        #[derive(Parser)]
        struct Cli {
            #[command(flatten)]
            args: Args,
        }
        Cli::parse_from(std::iter::once("viralshorts").chain(lista.iter().copied())).args
    }

    // -- argumentos y ajustes ------------------------------------------------------

    #[test]
    fn los_argumentos_pisan_la_configuracion() {
        let mut cfg = Config::default();
        apply_overrides(
            &mut cfg,
            &args(&["--clips", "5", "--min-duration", "20", "--max-duration", "50", "--head-skip", "10", "--tail-skip", "5", "--output", "salida", "--model", "tiny", "--language", "en", "--beam", "3", "--regions", "7"]),
        )
        .unwrap();
        assert_eq!((cfg.clips.num_clips, cfg.clips.min_duration, cfg.clips.max_duration), (5, 20.0, 50.0));
        assert_eq!((cfg.clips.head_skip, cfg.clips.tail_skip), (10.0, 5.0));
        assert_eq!((cfg.output.dir.as_str(), cfg.transcribe.model_size.as_str(), cfg.transcribe.language.as_deref()), ("salida", "tiny", Some("en")));
        assert_eq!((cfg.transcribe.beam_size, cfg.prescan.regions), (3, 7));
    }

    #[test]
    fn las_banderas_apagan_cosas() {
        let mut cfg = Config::default();
        apply_overrides(&mut cfg, &args(&["--no-llm", "--no-censor", "--no-title-card", "--no-context", "--no-metadata", "--no-reframe", "--no-jumpcuts", "--no-zoom", "--full-transcript", "--language", "auto"])).unwrap();
        assert!(!cfg.llm.enabled && !cfg.subtitles.censor_profanity && !cfg.title_card.enabled && !cfg.context.enabled);
        assert!(!cfg.metadata.enabled && !cfg.reframe.enabled && !cfg.jumpcuts.enabled && !cfg.zoom.enabled && !cfg.prescan.enabled);
        assert_eq!(cfg.transcribe.language, None);
    }

    #[test]
    fn set_va_despues_y_la_duracion_minima_no_puede_pasar_de_la_maxima() {
        let mut cfg = Config::default();
        apply_overrides(&mut cfg, &args(&["--clips", "5", "--set", "clips.num_clips=9", "--set", "subtitles.font_size=80"])).unwrap();
        assert_eq!((cfg.clips.num_clips, cfg.subtitles.font_size), (9, 80));

        let error = apply_overrides(&mut Config::default(), &args(&["--min-duration", "90", "--max-duration", "30"])).unwrap_err();
        assert!(error.to_string().contains("min_duration"));
        assert!(apply_overrides(&mut Config::default(), &args(&["--set", "no.existe=1"])).is_err());
    }

    #[test]
    fn candidatos_que_se_proponen_para_revisar() {
        let con = |clips: i64, propose: Option<i64>| {
            let mut cfg = Config::default();
            cfg.clips.num_clips = clips;
            proposed_count(&Args { propose, ..Default::default() }, &cfg)
        };
        assert_eq!(con(3, None), 9);
        assert_eq!(con(1, None), 6, "como minimo seis");
        assert_eq!(con(10, None), 20, "como maximo veinte");
        assert_eq!(con(3, Some(12)), 12);
        assert_eq!(con(5, Some(2)), 5, "nunca menos de los pedidos");
    }

    #[test]
    fn renders_a_la_vez() {
        let con = |workers: i64, total: usize| {
            let mut cfg = Config::default();
            cfg.output.render_workers = workers;
            render_workers(&cfg, total)
        };
        assert_eq!(con(3, 10), 3);
        assert_eq!(con(5, 2), 2, "no mas hilos que clips");
        assert_eq!(con(0, 1), 1);
        assert!(con(0, 5) >= 1);
    }

    // -- plan ------------------------------------------------------------------------

    fn plan_con(n: usize, marcados: usize) -> plan::Plan {
        let dir = tempfile::tempdir().unwrap();
        let ruta = dir.path().join("plan.json");
        let clips: Vec<Window> = (0..n).map(|i| Window::new(i as f64 * 100.0, i as f64 * 100.0 + 30.0, vec![Word::new("hola.", i as f64 * 100.0, i as f64 * 100.0 + 1.0)], "hola.")).collect();
        let fuente = PlanSource { ruta: "p.mp4".into(), titulo: "P".into(), duracion: 100.0, url: String::new() };
        let meta = HashMap::from([(0, ClipMetadata::new("Titulo", "GANCHO", "Desc.", &["a"]))]);
        plan::save(&ruta, &Config::default(), &fuente, &fuente, &clips, &meta, marcados).unwrap();
        plan::load(&ruta).unwrap()
    }

    #[test]
    fn que_clips_se_montan() {
        let plan = plan_con(5, 2);
        assert_eq!(wanted_numbers(&args(&[]), &plan), [1, 2]);
        assert_eq!(wanted_numbers(&args(&["--only", "4, 1,4,9,x,"]), &plan), [1, 4], "ordenados, sin repetir y sin los que no existen");
        assert!(wanted_numbers(&args(&["--only", "0,7"]), &plan).is_empty());
    }

    #[test]
    fn los_eventos_del_plan_para_la_interfaz() {
        let eventos = plan_events(&plan_con(3, 1));
        assert_eq!(eventos.len(), 3);
        assert_eq!((eventos[0]["n"].clone(), eventos[0]["titulo"].clone(), eventos[0]["seleccionado"].clone()), (json!(1), json!("Titulo"), json!(true)));
        assert_eq!((eventos[1]["titulo"].clone(), eventos[1]["seleccionado"].clone(), eventos[1]["hashtags"].clone()), (json!(""), json!(false), json!([])));
        assert_eq!(eventos[0]["marca"], "0:00");
    }

    // -- textos de publicacion ------------------------------------------------------------

    #[test]
    fn sin_claude_los_textos_salen_por_extraccion() {
        let mut cfg = Config::default();
        cfg.llm.enabled = false;
        let mut juez = ClipJudge::new(cfg.llm.clone(), None);
        let palabras: Vec<Word> = "Nunca nadie te ha contado esto del dinero.".split(' ').enumerate().map(|(i, t)| Word::new(t, 10.0 + i as f64 * 0.4, 10.3 + i as f64 * 0.4)).collect();
        let clip = Window::new(10.0, 40.0, palabras, "Nunca nadie te ha contado esto del dinero.");
        let textos = metadata_for(&mut juez, &[clip.clone()], "Mi Programa #1 - Alguien", &cfg);
        assert!(!textos[&0].title.is_empty() && !textos[&0].hashtags.is_empty());

        cfg.metadata.enabled = false;
        assert!(metadata_for(&mut juez, &[clip], "x", &cfg).is_empty(), "con los textos apagados no se escribe nada");
        assert!(metadata_for(&mut juez, &[], "x", &cfg).is_empty());
    }

    #[test]
    fn la_carpeta_de_cada_ejecucion() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::default();
        let carpeta = run_output_dir(&cfg, dir.path(), "Mi Podcast", "20260101_101500");
        assert_eq!(carpeta, dir.path().join("mi-podcast_20260101_101500"));
        assert!(carpeta.is_dir());

        let mut plana = Config::default();
        plana.output.run_subfolder = false;
        assert_eq!(run_output_dir(&plana, dir.path(), "x", "t"), dir.path());
    }

    // -- ficheros que deja cada short -------------------------------------------------------

    #[test]
    fn el_json_y_el_txt_junto_al_video() {
        let dir = tempfile::tempdir().unwrap();
        let video = dir.path().join("short_3.mp4");
        let clip = Window { score: 0.81234, heat: 0.5, reason: "engancha".into(), ..Window::new(563.74, 583.06, vec![], "hola mundo") };
        let podcast = Source { path: "p.mp4".into(), title: "El Programa".into(), duration: 100.0, url: "https://youtu.be/x".into() };
        let cortes = jumpcuts::JumpCutPlan { keeps: vec![(0.0, 5.0), (7.0, 19.32)], source_duration: 19.32 };
        let meta = ClipMetadata::new("Un titulo", "GANCHO", "Desc.", &["podcast"]);

        write_sidecar(&video, &clip, Some(&meta), &podcast, &cortes);
        let datos: Value = serde_json::from_str(&std::fs::read_to_string(dir.path().join("short_3.json")).unwrap()).unwrap();
        assert_eq!(datos["video"], "short_3.mp4");
        assert_eq!(datos["fuente"], json!({"titulo": "El Programa", "url": "https://youtu.be/x", "inicio": 563.74, "fin": 583.06, "marca": "9:23"}));
        assert_eq!((datos["duracion_final"].clone(), datos["silencio_eliminado"].clone()), (json!(17.32), json!(2.0)));
        assert_eq!(datos["puntuacion"]["total"], 0.8123);
        assert_eq!(datos["publicacion"]["title"], "Un titulo");
        assert_eq!(std::fs::read_to_string(dir.path().join("short_3.txt")).unwrap(), "Un titulo\n\nDesc.\n\n#podcast\n");

        // Sin textos solo queda el .json, y una fuente local no tiene URL.
        let sin = Source { url: String::new(), ..podcast };
        write_sidecar(&dir.path().join("short_4.mp4"), &clip, None, &sin, &cortes);
        assert!(!dir.path().join("short_4.txt").exists());
        let otro: Value = serde_json::from_str(&std::fs::read_to_string(dir.path().join("short_4.json")).unwrap()).unwrap();
        assert!(otro["fuente"]["url"].is_null() && otro.get("publicacion").is_none());
    }

    #[test]
    fn el_resumen_final() {
        let bien: Vec<RenderResult> = vec![(1, Ok(PathBuf::from("a.mp4"))), (2, Err("fallo".into()))];
        assert_eq!(report_results(&bien, 2, 1.0), 0, "con que salga uno vale");
        assert_eq!(report_results(&[(1, Err("x".into()))], 1, 1.0), 1);
        assert_eq!(report_results(&[], 0, 0.0), 1);
    }

    #[test]
    fn sin_podcast_y_fondo_no_se_ejecuta() {
        assert_eq!(run(&Args::default()), 2);
        assert_eq!(run(&Args { podcast: Some("x".into()), ..Default::default() }), 2);
    }
}
