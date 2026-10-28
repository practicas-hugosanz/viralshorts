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
//! Elegir los trozos a mano (la interfaz lo hace con su linea de tiempo):
//!
//! ```text
//! viralshorts --podcast <URL> --prepare
//! viralshorts --podcast <URL> --background <URL> --moments 754-790,2462-2493
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
    /// Solo descargar el podcast y preparar su linea de tiempo para elegir a mano.
    #[arg(long, help_heading = "Elegir a mano")]
    pub prepare: bool,
    /// Montar exactamente estos trozos, en segundos: 754-790,2462-2493.
    #[arg(long, value_name = "INICIO-FIN,...", help_heading = "Elegir a mano")]
    pub moments: Option<String>,
    /// Rehacer un short ya montado con su short_N.retoque.json (texto, recorte o estilo cambiados).
    #[arg(long, value_name = "FICHERO", help_heading = "Montaje en dos tiempos")]
    pub remake: Option<PathBuf>,
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
    if cfg.layout.mode != "stack" && cfg.layout.mode != "duo" && cfg.title_card.anchor == "center" {
        cfg.title_card.anchor = "top".into();
    }
    if !["stack", "full", "center", "duo"].contains(&cfg.layout.mode.as_str()) {
        return Err(ConfigError::Invalid(format!("layout.mode '{}' no existe: usa stack, full, center o duo.", cfg.layout.mode)));
    }
    if !["sequence", "random"].contains(&cfg.background.start.as_str()) {
        return Err(ConfigError::Invalid(format!("background.start '{}' no existe: usa sequence o random.", cfg.background.start)));
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
    if args.from_plan.is_none() && args.remake.is_none() && args.podcast.as_deref().is_none_or(|p| p.trim().is_empty()) {
        log::error!("Hace falta --podcast (o --from-plan).");
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

    let code = match (&args.from_plan, &args.moments) {
        _ if args.remake.is_some() => run_remake(args, args.remake.as_deref().unwrap_or(Path::new("")), &mut cfg, &work_dir, started),
        (Some(path), _) => run_from_plan(args, path, &mut cfg, &work_dir, started),
        _ if args.prepare => run_prepare(args, &cfg, &work_dir),
        (None, Some(moments)) => match parse_moments(moments) {
            Ok(moments) => run_manual(args, &cfg, &work_dir, &output_dir, &moments, started),
            Err(error) => {
                log::error!("{error}");
                2
            }
        },
        (None, None) => run_new(args, &cfg, &work_dir, &output_dir, started),
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
    /// Mas fondos, si se pidieron varios: los shorts se los van turnando.
    extra_backgrounds: Vec<SourceVideo>,
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
    clips_event(&selection.clips);

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
            &selection.extra_backgrounds.iter().map(PlanSource::from_video).collect::<Vec<_>>(),
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
    let backgrounds: Vec<Source> = std::iter::once(background).chain(selection.extra_backgrounds.iter().map(Source::from)).collect();
    let results = render_all(&selection.clips, &written, &podcast, &backgrounds, cfg, work_dir, &output_dir, &numbers, Some(&selection.energy));

    finish(cfg, &output_dir);
    free_source(cfg, work_dir, &podcast, &results);
    report_results(&results, selection.clips.len(), started.elapsed().as_secs_f64())
}

/// Los clips elegidos, para que la interfaz los ensene antes de montarlos.
fn clips_event(clips: &[Window]) {
    progress::result(
        "clips",
        json!({"clips": clips.iter().enumerate().map(|(i, c)| json!({
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
    let (background, extra_backgrounds) = fetch_backgrounds(args, cfg, work_dir, &podcast)?;

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
    translate_clips(cfg, &podcast.path, podcast.duration, &mut picked, &mut judge);
    Ok(Selection { clips: picked, podcast, background, extra_backgrounds, judge, energy })
}

/// El video de fondo y, si se pidieron varios, los demas.
fn fetch_backgrounds(args: &Args, cfg: &Config, work_dir: &Path, podcast: &SourceVideo) -> Result<(SourceVideo, Vec<SourceVideo>), String> {
    let wanted = split_backgrounds(args.background.as_deref().unwrap_or(""));
    // "Solo el podcast" y "Dos personas" no usan fondo: no hace falta pedirlo.
    let background = match wanted.first() {
        Some(first) => download::fetch(first, &work_dir.join("background"), &cfg.download).map_err(|e| e.to_string())?,
        None if !layout_needs_background(cfg) => podcast.clone(),
        None => return Err("Falta el video de fondo. Pega un enlace en «Vídeo de fondo», o elige la composición «Solo el pódcast».".into()),
    };
    let extra_backgrounds = wanted
        .iter()
        .enumerate()
        .skip(1)
        .map(|(i, url)| download::fetch(url, &work_dir.join(format!("background_{}", i + 1)), &cfg.download).map_err(|e| e.to_string()))
        .collect::<Result<Vec<_>, String>>()?;
    Ok((background, extra_backgrounds))
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

    let outcome = (|| -> Result<ClipWords, String> {
        let mut refine = cfg.transcribe.clone();
        refine.model_size = model.to_string();
        refine.beam_size = refine.beam_size.max(5);
        let engine = WhisperCpp::discover(&ffmpeg::project_root()).map_err(|e| e.to_string())?;
        let mut transcriber = Transcriber::new(refine, &cfg.paths.cache_dir, Box::new(engine)).map_err(|e| e.to_string())?;
        let regions: Vec<(f64, f64)> = clips.iter().map(|c| ((c.start - 1.0).max(0.0), (c.end + 1.0).min(duration))).collect();
        let transcript = transcriber.transcribe_regions(podcast, duration, &regions).map_err(|e| e.to_string())?;
        // Una pasada que devuelve la mitad de las palabras ha ido mal.
        Ok(words_per_clip(&transcript, clips, |found, clip| found * 2 >= clip.words.len()))
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

/// La composicion usa el video de fondo (dividido o franja central).
pub fn layout_needs_background(cfg: &Config) -> bool {
    !matches!(cfg.layout.mode.as_str(), "full" | "duo")
}

/// Los videos de fondo pedidos: uno, o varios separados por | o por lineas.
pub fn split_backgrounds(text: &str) -> Vec<String> {
    text.split(['|', '\n']).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect()
}

/// Por clip: sus palabras nuevas y su texto, o None si la pasada no valio.
type ClipWords = Vec<Option<(Vec<crate::words::Word>, String)>>;

/// Las palabras de una pasada de whisper por zonas, repartidas por clip.
/// `enough` decide si las encontradas valen (cuantas, frente al clip).
fn words_per_clip(
    transcript: &crate::words::Transcript,
    clips: &[Window],
    enough: impl Fn(usize, &Window) -> bool,
) -> ClipWords {
    let all: Vec<&crate::words::Word> = transcript.segments.iter().flat_map(|s| s.words.iter()).collect();
    clips
        .iter()
        .map(|clip| {
            let mut words: Vec<crate::words::Word> =
                all.iter().filter(|w| w.start >= clip.start - 0.25 && w.end <= clip.end + 0.25).map(|w| (*w).clone()).collect();
            words.sort_by(|a, b| a.start.total_cmp(&b.start));
            words.dedup_by(|a, b| (a.start - b.start).abs() < 0.05 && a.text == b.text);
            (words.len() >= 3 && enough(words.len(), clip)).then(|| {
                let text = words.iter().map(|w| w.text.as_str()).collect::<Vec<_>>().join(" ");
                (words, text)
            })
        })
        .collect()
}

/// Subtitulos en otro idioma: cambia las palabras (y el texto, del que salen
/// titulo y gancho) de cada clip por las traducidas. Al ingles traduce whisper
/// el mismo audio; a los demas idiomas, Claude frase a frase. Si no se puede,
/// se queda el idioma original y se avisa.
fn translate_clips(cfg: &Config, podcast: &Path, duration: f64, clips: &mut [Window], judge: &mut ClipJudge) {
    let target = cfg.subtitles.translate_to.trim().to_lowercase();
    if target.is_empty() || clips.is_empty() {
        return;
    }
    let source = cfg.transcribe.language.clone().unwrap_or_default().to_lowercase();
    if source == target {
        return;
    }
    let Some(name) = crate::translate::language_name(&target) else {
        log::warn!("Idioma de subtitulos desconocido ({target}); van en el original.");
        return;
    };
    progress::stage("transcripcion", &format!("Traduciendo los subtitulos al {name}"));

    if target == "en" {
        // whisper traduce al ingles con el modelo normal; los "turbo" no saben traducir.
        let model = [cfg.transcribe.refine_model.trim(), cfg.transcribe.model_size.trim()]
            .into_iter()
            .find(|m| !m.is_empty() && !m.contains("turbo"))
            .unwrap_or("medium")
            .to_string();
        let outcome = (|| -> Result<ClipWords, String> {
            let mut settings = cfg.transcribe.clone();
            settings.model_size = model.clone();
            settings.translate = true;
            settings.corrections.clear(); // estan escritas para el idioma original
            let engine = WhisperCpp::discover(&ffmpeg::project_root()).map_err(|e| e.to_string())?;
            let mut transcriber = Transcriber::new(settings, &cfg.paths.cache_dir, Box::new(engine)).map_err(|e| e.to_string())?;
            let regions: Vec<(f64, f64)> = clips.iter().map(|c| ((c.start - 1.0).max(0.0), (c.end + 1.0).min(duration))).collect();
            let transcript = transcriber.transcribe_regions(podcast, duration, &regions).map_err(|e| e.to_string())?;
            // Traducido salen menos palabras que en espanol: basta con un tercio.
            Ok(words_per_clip(&transcript, clips, |found, clip| found * 3 >= clip.words.len()))
        })();
        match outcome {
            Ok(results) => {
                let mut done = 0;
                for (clip, result) in clips.iter_mut().zip(results) {
                    if let Some((words, text)) = result {
                        clip.words = words;
                        clip.text = text;
                        done += 1;
                    }
                }
                log::info!("Subtitulos traducidos al ingles (whisper, modelo {model}) en {done} de {} clips.", clips.len());
            }
            Err(error) => log::warn!("No se pudo traducir al ingles ({error}); los subtitulos van en el original."),
        }
        return;
    }

    if !cfg.llm.enabled || !judge.available() {
        log::warn!("Para traducir al {name} hace falta Claude (al ingles no). Los subtitulos van en el original.");
        progress::result(
            "aviso",
            json!({"titulo": format!("Sin traducir al {name}"), "texto": "Traducir a ese idioma necesita la clave de Claude. Al inglés se traduce sin ella."}),
        );
        return;
    }
    let english = crate::translate::english_name(&target).unwrap_or("English");
    let mut done = 0;
    for clip in clips.iter_mut() {
        let spans = crate::translate::phrases(&clip.words, 12);
        let originals: Vec<String> =
            spans.iter().map(|&(a, b)| clip.words[a..b].iter().map(|w| w.text.as_str()).collect::<Vec<_>>().join(" ")).collect();
        let translated = judge.translate_phrases(&originals, english);
        if translated.iter().all(Option::is_none) {
            continue;
        }
        clip.words = crate::translate::assemble(&clip.words, &spans, &translated);
        clip.text = clip.words.iter().map(|w| w.text.as_str()).collect::<Vec<_>>().join(" ");
        done += 1;
    }
    log::info!("Subtitulos traducidos al {name} (Claude) en {done} de {} clips.", clips.len());
}

// ----------------------------------------------------------------------------
// Render a partir de un plan revisado
// ----------------------------------------------------------------------------

// ----------------------------------------------------------------------------
// Elegir a mano: preparar el podcast y montar los trozos marcados
// ----------------------------------------------------------------------------

/// Primer paso de elegir a mano: deja el podcast descargado y escribe lo que
/// la linea de tiempo de la interfaz necesita (lo mas visto y el volumen).
fn run_prepare(args: &Args, cfg: &Config, work_dir: &Path) -> i32 {
    progress::stage("descarga", "Descargando el pódcast");
    let origin = args.podcast.as_deref().unwrap_or("");
    let podcast = match download::fetch(origin, &work_dir.join("podcast"), &cfg.download) {
        Ok(podcast) => podcast,
        Err(error) => {
            log::error!("No se pudo descargar el podcast: {error}");
            return 1;
        }
    };

    // El volumen ayuda a cortar en una pausa; si no se puede sacar, la linea
    // de tiempo sale sin onda y se elige igual.
    progress::stage("analisis", "Preparando la línea de tiempo");
    let waves = match audio_energy::analyse(&podcast.path, podcast.duration) {
        Ok(track) => track.level.iter().map(|level| (level * 99.0).round() as u8).collect(),
        Err(error) => {
            log::warn!("Sin onda de volumen en la linea de tiempo ({error}).");
            Vec::new()
        }
    };
    let path = std::path::absolute(&podcast.path).unwrap_or(podcast.path.clone());
    let data = timeline_data(&podcast, origin, &path, &waves, audio_energy::HOP_SECONDS);
    let data_path = std::path::absolute(work_dir.join("editor.json")).unwrap_or(work_dir.join("editor.json"));
    if let Err(error) = std::fs::write(&data_path, data.to_string()) {
        log::error!("No se pudo guardar {}: {error}", data_path.display());
        return 1;
    }
    progress::result(
        "fuente",
        json!({
            "ruta": path.to_string_lossy(),
            "titulo": podcast.title,
            "duracion": round(podcast.duration, 2),
            "origen": origin,
            "datos": data_path.to_string_lossy(),
        }),
    );
    log::info!("Podcast listo para elegir los trozos a mano ({}).", hms(podcast.duration));
    0
}

/// Lo que pinta la linea de tiempo: lo mas visto de YouTube (si lo hay) y el
/// volumen, de 0 a 99, una muestra cada `hop` segundos.
pub fn timeline_data(podcast: &SourceVideo, origin: &str, path: &Path, waves: &[u8], hop: f64) -> Value {
    json!({
        "ruta": path.to_string_lossy(),
        "titulo": podcast.title,
        "duracion": round(podcast.duration, 2),
        "origen": origin,
        "visto": podcast.heatmap.iter().map(|h| json!([round(h.start, 2), round(h.end, 2), round(h.value, 3)])).collect::<Vec<_>>(),
        "paso": hop,
        "onda": waves,
    })
}

/// "754-790,2462.5-2493" en pares (inicio, fin), en el orden en que vienen.
pub fn parse_moments(text: &str) -> Result<Vec<(f64, f64)>, String> {
    let moments: Vec<(f64, f64)> = text
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(|part| {
            let (start, end) = part.split_once('-').ok_or_else(|| format!("Trozo mal escrito: '{part}' (se espera INICIO-FIN en segundos)."))?;
            let parse = |value: &str| value.trim().parse::<f64>().ok().filter(|v| v.is_finite() && *v >= 0.0);
            match (parse(start), parse(end)) {
                (Some(start), Some(end)) if end - start >= 1.0 => Ok((start, end)),
                (Some(_), Some(_)) => Err(format!("El trozo '{part}' dura menos de un segundo.")),
                _ => Err(format!("Trozo mal escrito: '{part}' (se espera INICIO-FIN en segundos).")),
            }
        })
        .collect::<Result<_, _>>()?;
    if moments.is_empty() {
        return Err("--moments no trae ningun trozo.".into());
    }
    Ok(moments)
}

/// Monta exactamente los trozos elegidos a mano: sin buscar ni puntuar nada,
/// solo transcribe esos segundos y los monta tal cual.
fn run_manual(args: &Args, cfg: &Config, work_dir: &Path, output_dir: &Path, moments: &[(f64, f64)], started: Instant) -> i32 {
    check_fonts(cfg);
    let mut judge = ClipJudge::new(cfg.llm.clone(), Some(Path::new(&cfg.paths.cache_dir)));
    let chosen = (|| -> Result<(Vec<Window>, SourceVideo, SourceVideo, Vec<SourceVideo>), String> {
        progress::stage("descarga", "Descargando los videos");
        let podcast = download::fetch(args.podcast.as_deref().unwrap_or(""), &work_dir.join("podcast"), &cfg.download).map_err(|e| e.to_string())?;
        let (background, extra_backgrounds) = fetch_backgrounds(args, cfg, work_dir, &podcast)?;
        let mut clips = manual_windows(moments, podcast.duration);
        if clips.is_empty() {
            return Err("Los trozos elegidos se salen del vídeo.".into());
        }

        progress::stage("transcripcion", "Transcribiendo los trozos elegidos");
        let engine = WhisperCpp::discover(&ffmpeg::project_root()).map_err(|e| e.to_string())?;
        let mut transcriber = Transcriber::new(cfg.transcribe.clone(), &cfg.paths.cache_dir, Box::new(engine)).map_err(|e| e.to_string())?;
        let regions: Vec<(f64, f64)> = clips.iter().map(|c| ((c.start - 1.0).max(0.0), (c.end + 1.0).min(podcast.duration))).collect();
        let transcript = transcriber.transcribe_regions(&podcast.path, podcast.duration, &regions).map_err(|e| e.to_string())?;
        // Un trozo sin voz (musica, un silencio) se monta igual, sin subtitulos.
        let found_words = words_per_clip(&transcript, &clips, |_, _| true);
        for (clip, found) in clips.iter_mut().zip(found_words) {
            if let Some((words, text)) = found {
                clip.words = words;
                clip.text = text;
            }
        }
        refine_clips(cfg, &podcast.path, podcast.duration, &mut clips);
        translate_clips(cfg, &podcast.path, podcast.duration, &mut clips, &mut judge);
        Ok((clips, podcast, background, extra_backgrounds))
    })();
    let (clips, podcast, background, extra_backgrounds) = match chosen {
        Ok(chosen) => chosen,
        Err(error) => {
            log::error!("No se pudieron preparar los trozos elegidos: {error}");
            return 1;
        }
    };

    report_selection(&clips);
    clips_event(&clips);
    let podcast = Source::from(&podcast);
    progress::stage("metadatos", "Escribiendo titulos y hashtags");
    let written = metadata_for(&mut judge, &clips, &podcast.title, cfg);
    let output_dir = run_output_dir(cfg, output_dir, &podcast.title, &chrono::Local::now().format("%Y%m%d_%H%M%S").to_string());
    let numbers: Vec<usize> = (1..=clips.len()).collect();
    let backgrounds: Vec<Source> = std::iter::once(Source::from(&background)).chain(extra_backgrounds.iter().map(Source::from)).collect();
    let results = render_all(&clips, &written, &podcast, &backgrounds, cfg, work_dir, &output_dir, &numbers, None);

    finish(cfg, &output_dir);
    free_source(cfg, work_dir, &podcast, &results);
    report_results(&results, clips.len(), started.elapsed().as_secs_f64())
}

/// Los trozos como clips, recortados a lo que dura el video.
pub fn manual_windows(moments: &[(f64, f64)], duration: f64) -> Vec<Window> {
    moments
        .iter()
        .map(|&(start, end)| (start.max(0.0), if duration > 0.0 { end.min(duration) } else { end }))
        .filter(|(start, end)| end - start >= 1.0)
        .map(|(start, end)| {
            let mut clip = Window::new(start, end, Vec::new(), String::new());
            clip.reason = "Elegido a mano".into();
            clip.score = 1.0;
            clip
        })
        .collect()
}

fn run_from_plan(args: &Args, plan_path: &Path, cfg: &mut Config, work_dir: &Path, started: Instant) -> i32 {
    let saved = match plan::load(plan_path) {
        Ok(saved) => saved,
        Err(error) => {
            log::error!("{error}");
            return 2;
        }
    };

    match config_from_plan(&saved, args) {
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

    let (podcast, background, extra) = match plan_sources(&saved, cfg, work_dir) {
        Ok(sources) => sources,
        Err(error) => {
            log::error!("{error}");
            return 2;
        }
    };

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
    let backgrounds: Vec<Source> = std::iter::once(background).chain(extra).collect();
    let results = render_all(&clips, &written, &podcast, &backgrounds, cfg, work_dir, &output_dir, &wanted, None);

    finish(cfg, &output_dir);
    free_source(cfg, work_dir, &podcast, &results);
    report_results(&results, clips.len(), started.elapsed().as_secs_f64())
}

/// La configuracion del plan manda: es con la que se eligieron los clips.
/// Encima se aplican los argumentos de esta llamada (carpeta de salida, --set
/// de estilo), que es lo que el usuario acaba de pedir.
fn config_from_plan(saved: &plan::Plan, args: &Args) -> Result<Config, ConfigError> {
    let mut fresh = Config::default();
    fresh.merge(&saved.config, "plan")?;
    apply_overrides(&mut fresh, args)?;
    Ok(fresh)
}

/// El podcast y los fondos de un plan, descargandolos otra vez si ya no estan.
fn plan_sources(saved: &plan::Plan, cfg: &Config, work_dir: &Path) -> Result<(Source, Source, Vec<Source>), String> {
    let mut podcast = Source::from(&saved.podcast);
    // Un plan sin fondo (composicion sin fondo): se usa el propio podcast.
    let own_background = !(saved.background.ruta.is_empty() && saved.background.url.is_empty());
    let mut background = Source::from(if own_background { &saved.background } else { &saved.podcast });
    let mut extra: Vec<Source> = saved.extra_backgrounds.iter().map(Source::from).collect();
    let mut sources: Vec<(&mut Source, String, &str)> = vec![(&mut podcast, "podcast".into(), "podcast")];
    if own_background {
        sources.push((&mut background, "background".into(), "video de fondo"));
    }
    sources.extend(extra.iter_mut().enumerate().map(|(i, s)| (s, format!("background_{}", i + 2), "video de fondo")));
    for (source, base, label) in sources {
        if source.path.is_file() {
            continue;
        }
        // Lo normal es que se borrase al terminar el montaje anterior para no
        // ocupar disco: si vino de un enlace, se baja otra vez.
        if source.url.is_empty() {
            return Err(format!("El {label} ya no esta en {}. Vuelve a elegirlo y monta de nuevo.", source.path.display()));
        }
        progress::stage("descarga", "Descargando los videos");
        log::info!("El {label} ya no esta en el disco: se vuelve a descargar.");
        let video = download::fetch(&source.url, &work_dir.join(base), &cfg.download).map_err(|e| format!("No se pudo volver a descargar el {label}: {e}"))?;
        source.path = video.path;
    }
    if !own_background {
        background = podcast.clone();
    }
    Ok((podcast, background, extra))
}

/// Rehace un short ya montado desde su `short_N.retoque.json`, con el texto,
/// el recorte o el estilo que se le hayan cambiado: sin elegir ni transcribir
/// nada, en su misma carpeta y con su mismo numero (lo sustituye).
fn run_remake(args: &Args, path: &Path, cfg: &mut Config, work_dir: &Path, started: Instant) -> i32 {
    let saved = match plan::load(path) {
        Ok(saved) if saved.clips.len() == 1 => saved,
        Ok(_) => {
            log::error!("{} no es el plan de un solo short.", path.display());
            return 2;
        }
        Err(error) => {
            log::error!("{error}");
            return 2;
        }
    };
    match config_from_plan(&saved, args) {
        Ok(fresh) => *cfg = fresh,
        Err(error) => {
            log::error!("El short trae ajustes que esta version no entiende: {error}");
            return 2;
        }
    }
    let Some(folder) = path.parent().filter(|f| f.is_dir()).map(Path::to_path_buf) else {
        log::error!("La carpeta del short ya no existe.");
        return 2;
    };
    let (number, turn) = plan::single_slot(path).unwrap_or((1, 1));
    check_fonts(cfg);
    let (podcast, background, _) = match plan_sources(&saved, cfg, work_dir) {
        Ok(sources) => sources,
        Err(error) => {
            log::error!("{error}");
            return 2;
        }
    };

    progress::result("salida", json!({"ruta": std::path::absolute(&folder).unwrap_or(folder.clone()).to_string_lossy()}));
    let clip = saved.clips[0].to_window();
    let meta = saved.clips[0].meta.clone();
    log::info!("Rehaciendo el short {number} ({}).", clip.describe());
    progress::stage("render", &format!("Rehaciendo el short {number}"));
    let duration = ffmpeg::probe(&background.path).map(|i| i.duration).unwrap_or(background.duration);
    let outcome = render_one(&clip, number, turn, meta.as_ref(), &podcast, &background, duration, cfg, work_dir, &folder, None);
    clip_event(number, &outcome, meta.as_ref());
    let results = vec![(number, outcome)];
    finish(cfg, &folder);
    free_source(cfg, work_dir, &podcast, &results);
    report_results(&results, 1, started.elapsed().as_secs_f64())
}

/// Le dice a la interfaz como ha ido un short.
fn clip_event(number: usize, outcome: &Result<PathBuf, String>, meta: Option<&ClipMetadata>) {
    match outcome {
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
}

/// Al terminar, borra el video descargado del podcast: pesa cientos de MB y
/// los shorts ya estan hechos. Solo si ha salido al menos uno (si todo falla,
/// se deja para reintentar sin descargar) y solo la descarga de la carpeta de
/// trabajo: un fichero propio del usuario no se toca nunca. El fondo se queda:
/// suele ser el mismo de una vez para otra.
fn free_source(cfg: &Config, work_dir: &Path, podcast: &Source, results: &[RenderResult]) {
    if !cfg.output.delete_source || !results.iter().any(|(_, r)| r.is_ok()) {
        return;
    }
    let base = work_dir.join("podcast");
    let same_dir = |a: &Path, b: &Path| std::fs::canonicalize(a).ok().zip(std::fs::canonicalize(b).ok()).is_some_and(|(a, b)| a == b);
    let downloaded = podcast.path.file_stem().is_some_and(|s| s == "podcast") && podcast.path.parent().is_some_and(|dir| same_dir(dir, work_dir));
    if !downloaded {
        return;
    }
    let freed = download::discard(&base);
    if freed > 0 {
        log::info!(
            "Borrado el video descargado ({:.1} MB) para no ocupar disco. Si vuelves a montar algo de este podcast se descarga otra vez.",
            freed as f64 / 1e6
        );
    }
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
                "palabras": clip.words.iter().map(|w| w.text.as_str()).collect::<Vec<_>>().join(" "),
                "recorte": {
                    "inicio": round(clip.original.as_ref().map_or(0.0, |o| clip.start - o.start), 2),
                    "fin": round(clip.original.as_ref().map_or(0.0, |o| o.end - clip.end), 2),
                },
                "editado": clip.original.is_some(),
                "estilo": clip.style,
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
    backgrounds: &[Source],
    cfg: &Config,
    work_dir: &Path,
    output_dir: &Path,
    numbers: &[usize],
    energy: Option<&EnergyTrack>,
) -> Vec<RenderResult> {
    let durations: Vec<f64> = backgrounds.iter().map(|b| ffmpeg::probe(&b.path).map(|i| i.duration).unwrap_or(b.duration)).collect();
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

                // Con varios fondos, el short N usa el N-esimo (en rotacion) y
                // cada fondo sigue por donde se quedo en su vuelta anterior.
                let which = (number - 1) % backgrounds.len().max(1);
                let turn = (number - 1) / backgrounds.len().max(1) + 1;
                let outcome =
                    render_one(clip, number, turn, meta, podcast, &backgrounds[which], durations[which], cfg, work_dir, output_dir, energy);
                clip_event(number, &outcome, meta);
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
    // Vuelta de su fondo: con un solo fondo es el numero del clip.
    turn: usize,
    meta: Option<&ClipMetadata>,
    podcast: &Source,
    background: &Source,
    background_duration: f64,
    cfg: &Config,
    work_dir: &Path,
    output_dir: &Path,
    energy: Option<&EnergyTrack>,
) -> Result<PathBuf, String> {
    let base_cfg = cfg;
    // La plantilla de subtitulos puede ser propia de este clip.
    let own_cfg;
    let cfg = if clip.style.is_empty() || clip.style == cfg.subtitles.style {
        cfg
    } else {
        let mut copy = cfg.clone();
        copy.subtitles.style = clip.style.clone();
        own_cfg = copy;
        &own_cfg
    };

    // 1. Palabras del clip, con el tiempo relativo a su inicio.
    let clip_words: Vec<_> = clip.words.iter().map(|w| w.shifted(-clip.start)).collect();

    // 2. Que silencios se quitan, y como afecta eso a los tiempos.
    let cuts = jumpcuts::plan(&clip_words, clip.duration(), &cfg.jumpcuts);
    let mut words = cuts.remap_words(&clip_words);

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
    // Con dos personas en plano, cada palabra sabe quien la dice (por su
    // momento ANTES de quitar silencios, que es el reloj del seguimiento).
    if let Some(track) = track.as_ref().filter(|t| t.duo.is_some() && !t.speakers.is_empty()) {
        for (word, original) in words.iter_mut().zip(&clip_words) {
            word.speaker = track.speaker_at((original.start + original.end) / 2.0);
        }
    }
    if cfg.layout.mode == "duo" && track.as_ref().and_then(|t| t.duo).is_none() {
        log::info!("Clip {number}: no hay dos personas en plano a la vez; va con el podcast a pantalla completa.");
    }

    // 4. Subtitulos + rotulo de gancho.
    let title_text = metadata::card_text(&podcast.title, meta.map_or("", |m| m.title.as_str()), cfg);
    let hook_text = meta.map_or("", |m| m.hook.as_str());
    let mut ass_name = None;
    if cfg.subtitles.enabled || !cfg.brand.handle.trim().is_empty() || cfg.end_card.enabled {
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
        subtitles::append_end_card(&mut content, &cfg.end_card, &cfg.brand.handle, cfg.layout.width, cfg.layout.height, cuts.output_duration(), &cfg.subtitles);
        let name = format!("short_{number}.ass");
        subtitles::write_ass(&content, &work_dir.join(&name)).map_err(|e| format!("No se pudo escribir {name}: {e}"))?;
        ass_name = Some(name);
    }

    // 5. Punto de entrada del fondo, distinto para cada clip y con vuelta al
    //    principio si el gameplay se acaba.
    let usable = (background_duration - clip.duration()).max(1.0);
    let offset = if cfg.background.start == "random" {
        // Un punto cualquiera, pero siempre el mismo para el mismo clip: rehacer
        // un short no le cambia el fondo.
        let mixed = (clip.start.to_bits() ^ (number as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        ((mixed >> 11) as f64 / (1u64 << 53) as f64) * usable
    } else {
        ((turn as f64 - 1.0) * clip.duration()) % usable
    };

    // Efectos de sonido: el pitido tapa las palabrotas; el whoosh acompana al rotulo.
    let beeps = if cfg.audio.censor_beep && cfg.subtitles.censor_profanity { subtitles::censored_intervals(&words, &cfg.subtitles) } else { Vec::new() };
    let mut whooshes = Vec::new();
    if cfg.audio.sfx {
        let card = cfg.title_card.enabled && !title_text.is_empty();
        let hook = cfg.hook.enabled && !hook_text.is_empty();
        if card || hook {
            whooshes.push(0.0);
        }
        if card && hook && cfg.title_card.duration > 0.3 {
            whooshes.push(cfg.title_card.duration.min(cuts.output_duration()));
        }
    }

    let emojis = subtitles::emoji_cues(&words, &cfg.subtitles, cfg.layout.width, cfg.layout.height);
    let output = output_dir.join(format!("short_{number}.mp4"));
    let request = RenderRequest {
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
        beeps,
        whooshes,
        emojis,
    };
    render::render(&request, cfg).map_err(|e: FfmpegError| e.to_string())?;

    // Para terminarlo en CapCut o Premiere: los subtitulos aparte (.srt) y el
    // mismo video sin ningun texto encima.
    if cfg.output.export_srt && !words.is_empty() {
        let srt = subtitles::srt(&words, &cfg.subtitles);
        if let Err(error) = std::fs::write(output_dir.join(format!("short_{number}.srt")), srt) {
            log::warn!("No se pudo guardar el .srt del clip {number}: {error}");
        }
    }
    if cfg.output.export_clean {
        let mut plain = cfg.clone();
        plain.progress_bar.enabled = false;
        plain.brand.logo.clear();
        let clean = RenderRequest {
            ass_name: None,
            emojis: Vec::new(),
            output: output_dir.join(format!("sin-subtitulos_{number}.mp4")),
            ..request
        };
        if let Err(error) = render::render(&clean, &plain) {
            log::warn!("No se pudo montar la version sin subtitulos del clip {number}: {error}");
        }
    }

    if cfg.output.thumbnail {
        let cover = (cfg.output.cover_text && !hook_text.is_empty())
            .then(|| render::extract_cover(&output, cfg.output.thumbnail_at, hook_text, cfg, work_dir))
            .flatten();
        if cover.is_none() {
            render::extract_thumbnail(&output, cfg.output.thumbnail_at);
        }
    }

    write_sidecar(&output, clip, meta, podcast, &cuts);
    write_retouch(&output, clip, meta, podcast, background, number, turn, base_cfg);
    Ok(output)
}

/// Junto al short, lo necesario para retocarlo despues (ver `plan::save_single`).
#[allow(clippy::too_many_arguments)]
fn write_retouch(output: &Path, clip: &Window, meta: Option<&ClipMetadata>, podcast: &Source, background: &Source, number: usize, turn: usize, cfg: &Config) {
    let source = |s: &Source| PlanSource {
        ruta: std::path::absolute(&s.path).unwrap_or(s.path.clone()).to_string_lossy().into_owned(),
        titulo: s.title.clone(),
        duracion: round(s.duration, 2),
        url: s.url.clone(),
    };
    // Sin fondo propio (el fondo es el mismo podcast): el plan lo deja vacio.
    let background = if background.path == podcast.path { PlanSource::default() } else { source(background) };
    if let Err(error) = plan::save_single(&retouch_path(output), cfg, &source(podcast), &background, clip, meta, number, turn) {
        log::warn!("No se pudo guardar lo necesario para retocar {}: {error}", output.display());
    }
}

/// `short_3.mp4` -> `short_3.retoque.json`.
pub fn retouch_path(video: &Path) -> PathBuf {
    video.with_extension("retoque.json")
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

    // -- elegir a mano --------------------------------------------------------------

    #[test]
    fn los_trozos_se_leen_en_segundos_y_los_mal_escritos_se_explican() {
        assert_eq!(parse_moments("754-790, 2462.5-2493").unwrap(), vec![(754.0, 790.0), (2462.5, 2493.0)]);
        assert_eq!(parse_moments("10-20,").unwrap(), vec![(10.0, 20.0)]);
        assert!(parse_moments("").is_err());
        assert!(parse_moments("20-10").unwrap_err().contains("menos de un segundo"));
        assert!(parse_moments("1:00-2:00").unwrap_err().contains("mal escrito"));
        assert!(parse_moments("5").unwrap_err().contains("mal escrito"));
        let leidos = args(&["--podcast", "p", "--moments", "1-31"]);
        assert_eq!((leidos.moments.as_deref(), leidos.prepare), (Some("1-31"), false));
        assert!(args(&["--podcast", "p", "--prepare"]).prepare);
    }

    #[test]
    fn los_trozos_se_recortan_a_lo_que_dura_el_video() {
        let clips = manual_windows(&[(10.0, 40.0), (290.0, 330.0), (299.5, 320.0)], 300.0);
        assert_eq!(clips.iter().map(|c| (c.start, c.end)).collect::<Vec<_>>(), vec![(10.0, 40.0), (290.0, 300.0)], "el que queda en medio segundo fuera");
        assert!(clips.iter().all(|c| c.reason == "Elegido a mano" && c.words.is_empty()));
    }

    #[test]
    fn la_linea_de_tiempo_lleva_lo_mas_visto_y_la_onda() {
        let video = SourceVideo {
            path: PathBuf::from("w/podcast.mp4"),
            title: "Episodio".into(),
            duration: 3600.0,
            heatmap: vec![download::HeatPoint { start: 0.0, end: 36.0, value: 0.5 }],
            info: Value::Null,
        };
        let datos = timeline_data(&video, "https://youtu.be/x", Path::new("C:/w/podcast.mp4"), &[0, 50, 99], 0.25);
        assert_eq!((datos["titulo"].clone(), datos["duracion"].clone(), datos["paso"].clone()), (json!("Episodio"), json!(3600.0), json!(0.25)));
        assert_eq!(datos["visto"], json!([[0.0, 36.0, 0.5]]));
        assert_eq!(datos["onda"], json!([0, 50, 99]));
        assert_eq!(datos["origen"], "https://youtu.be/x");
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
    fn varios_fondos_separados_por_barras_o_lineas() {
        assert_eq!(split_backgrounds("a.mp4"), ["a.mp4"]);
        assert_eq!(split_backgrounds(" https://youtu.be/x | b.mp4

C:/c.mp4 "), ["https://youtu.be/x", "b.mp4", "C:/c.mp4"]);
        assert!(split_backgrounds("  ").is_empty());
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
        plan::save(&ruta, &Config::default(), &fuente, &fuente, &[], &clips, &meta, marcados).unwrap();
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
    fn sin_podcast_no_se_ejecuta() {
        assert_eq!(run(&Args::default()), 2);
        assert_eq!(run(&Args { podcast: Some("  ".into()), ..Default::default() }), 2);
        // Sin fondo ya no es un error de uso: depende de la composicion (y aqui
        // falla despues, porque "x" no existe).
        assert_ne!(run(&Args { podcast: Some("x".into()), ..Default::default() }), 0);
    }

    #[test]
    fn solo_las_composiciones_con_fondo_lo_piden() {
        let mut cfg = Config::default();
        assert!(layout_needs_background(&cfg));
        for (mode, needs) in [("stack", true), ("center", true), ("full", false), ("duo", false)] {
            cfg.layout.mode = mode.into();
            assert_eq!(layout_needs_background(&cfg), needs, "{mode}");
        }
    }
}
