//! Render final: UNA sola pasada de ffmpeg.
//!
//! Se construye un unico `filter_complex` que hace todo de golpe:
//!
//! ```text
//!     recorte -> jump cuts -> reencuadre con seguimiento de cara -> punch-in
//!     -> apilado vertical -> subtitulos -> normalizacion de volumen
//! ```
//!
//! El grafo se escribe en un fichero y se pasa con `-filter_complex_script`
//! (o `-/filter_complex`) para no chocar con el limite de longitud de la linea
//! de comandos de Windows ni con el escapado: las expresiones de reencuadre son
//! muy largas.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::config::{Config, OutputConfig};
use crate::ffmpeg::{self, even, FfmpegError, MediaInfo};
use crate::jumpcuts::JumpCutPlan;
use crate::words::Track;

type Result<T> = std::result::Result<T, FfmpegError>;

pub struct RenderRequest {
    pub podcast: PathBuf,
    pub background: PathBuf,
    pub start: f64,
    pub end: f64,
    pub background_offset: f64,
    pub plan: JumpCutPlan,
    pub track: Option<Track>,
    /// Nombre del .ass DENTRO de work_dir.
    pub ass_name: Option<String>,
    pub output: PathBuf,
    pub work_dir: PathBuf,
}

/// Lo que el grafo necesita saber de la version de ffmpeg instalada.
#[derive(Debug, Clone, Copy)]
pub struct GraphOptions {
    /// Hasta ffmpeg 7 `crop` necesitaba `eval=frame` para recalcular x/y en
    /// cada fotograma; en ffmpeg 8 es el comportamiento por defecto y la
    /// opcion ya no existe.
    pub crop_has_eval: bool,
}

impl GraphOptions {
    pub fn detect() -> Self {
        Self { crop_has_eval: ffmpeg::filter_supports("crop", "eval") }
    }
}

/// Un decimal de Rust con la misma forma que el de Python ("1.0", no "1").
fn fl(value: f64) -> String {
    format!("{value:?}")
}

/// Genera el short vertical final. Devuelve la ruta del fichero.
pub fn render(request: &RenderRequest, cfg: &Config) -> Result<PathBuf> {
    let podcast_info = ffmpeg::probe(&request.podcast)?;
    let background_info = ffmpeg::probe(&request.background)?;

    let layout = &cfg.layout;
    let top_height = even((layout.height as f64 * layout.top_ratio) as i64);
    let bottom_height = layout.height - top_height;
    let duration = request.plan.output_duration();

    let graph = build_graph(
        request,
        &podcast_info,
        &background_info,
        top_height,
        bottom_height,
        cfg,
        GraphOptions::detect(),
    );

    std::fs::create_dir_all(&request.work_dir).map_err(io_error)?;
    let stem = request.output.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let script_path = request.work_dir.join(format!("{stem}.filter"));
    let filter_args = ffmpeg::write_filter_script(&graph, &script_path).map_err(io_error)?;

    let mut args: Vec<String> = vec![
        // Podcast: buscar y recortar en la entrada es mucho mas rapido que
        // decodificarlo entero para quedarse con 40 segundos.
        "-ss".into(), format!("{:.3}", request.start),
        "-t".into(), format!("{:.3}", request.end - request.start),
        "-i".into(), abs(&request.podcast),
        // Fondo en bucle infinito: da igual que el gameplay sea mas corto que
        // la suma de todos los clips.
        "-stream_loop".into(), "-1".into(),
        "-ss".into(), format!("{:.3}", request.background_offset),
        "-i".into(), abs(&request.background),
    ];
    args.extend(filter_args);
    args.extend(["-map", "[vout]", "-map", "[aout]"].map(String::from));
    args.extend(video_encoder_args(&cfg.output, &resolve_encoder(&cfg.output)));
    args.extend([
        "-pix_fmt".into(), "yuv420p".into(),
        "-r".into(), layout.fps.to_string(),
        "-c:a".into(), "aac".into(),
        "-b:a".into(), cfg.output.audio_bitrate.clone(),
        "-ar".into(), "48000".into(),
        "-ac".into(), "2".into(),
        "-movflags".into(), "+faststart".into(),
        "-t".into(), format!("{duration:.3}"),
        "-shortest".into(),
        abs(&request.output),
    ]);

    if let Some(parent) = request.output.parent() {
        std::fs::create_dir_all(parent).map_err(io_error)?;
    }
    // cwd = work_dir para que el filtro subtitles reciba un nombre relativo sin
    // letra de unidad ni dos puntos, que son un infierno de escapar en Windows.
    let name = request.output.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    ffmpeg::run(&args, Some(&request.work_dir), Some(duration), &format!("render {name}"))?;

    log::info!("Listo: {} ({duration:.1}s)", request.output.display());
    Ok(request.output.clone())
}

fn io_error(e: std::io::Error) -> FfmpegError {
    FfmpegError(e.to_string())
}

fn abs(path: &Path) -> String {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf()).to_string_lossy().into_owned()
}

// ----------------------------------------------------------------------------
// Construccion del filtergraph
// ----------------------------------------------------------------------------

pub fn build_graph(
    request: &RenderRequest,
    podcast_info: &MediaInfo,
    background_info: &MediaInfo,
    top_height: i64,
    bottom_height: i64,
    cfg: &Config,
    options: GraphOptions,
) -> String {
    let fps = cfg.layout.fps;
    let width = cfg.layout.width;
    let mut chains: Vec<String> = Vec::new();

    // --- panel superior: el podcast ---------------------------------------
    let mut top = vec!["[0:v]setpts=PTS-STARTPTS".to_string(), format!("fps={fps}")];
    top.extend(square_pixels(podcast_info));
    if request.plan.enabled() {
        top.push(format!("select='{}'", select_expr(&request.plan.keeps)));
        top.push("setpts=N/FRAME_RATE/TB".into());
    }
    top.extend(reframe_filters(
        podcast_info,
        width,
        top_height,
        request.track.as_ref(),
        &request.plan,
        cfg,
        options,
    ));
    top.push("setsar=1".into());
    chains.push(format!("{}[top]", top.join(",")));

    // --- panel inferior: el gameplay --------------------------------------
    let mut bottom = vec!["[1:v]setpts=PTS-STARTPTS".to_string(), format!("fps={fps}")];
    bottom.extend(square_pixels(background_info));
    bottom.push(center_crop(background_info, width, bottom_height));
    bottom.extend(cover(width, bottom_height));
    bottom.push("setsar=1".into());
    chains.push(format!("{}[bottom]", bottom.join(",")));

    // --- apilado + subtitulos ---------------------------------------------
    let stack = "[top][bottom]vstack=inputs=2";
    match &request.ass_name {
        Some(ass) => {
            let mut subtitles = format!("subtitles=filename={ass}");
            if let Some(dir) = cfg.subtitles.fonts_dir.as_deref().filter(|d| !d.is_empty()) {
                subtitles.push_str(&format!(":fontsdir={}", ffmpeg::escape_filter_path(Path::new(dir))));
            }
            chains.push(format!("{stack},{subtitles},format=yuv420p[vout]"));
        }
        None => chains.push(format!("{stack},format=yuv420p[vout]")),
    }

    // --- audio: solo el del podcast, normalizado --------------------------
    let mut audio = vec!["[0:a]asetpts=PTS-STARTPTS".to_string()];
    if request.plan.enabled() {
        audio.push(format!("aselect='{}'", select_expr(&request.plan.keeps)));
        audio.push("asetpts=N/SR/TB".into());
    }
    if cfg.audio.loudnorm {
        // Sin esto los clips suenan mas bajos que el resto del feed y el
        // espectador percibe el video como "peor" sin saber por que.
        audio.push(format!(
            "loudnorm=I={}:TP={}:LRA={}",
            fl(cfg.audio.target_i),
            fl(cfg.audio.target_tp),
            fl(cfg.audio.target_lra)
        ));
    }
    audio.push("aresample=48000".into());
    audio.push("aformat=sample_fmts=fltp:channel_layouts=stereo".into());
    chains.push(format!("{}[aout]", audio.join(",")));

    chains.join(";\n") + "\n"
}

/// Filtros que convierten el plano 16:9 del podcast en el panel vertical.
///
/// El orden no es negociable:
///
///  1. `crop` recorta del original la mayor ventana que YA tiene la proporcion
///     del panel, siguiendo la cara. Recortar, y no encajar, evita que la gente
///     salga estirada.
///  2. `scale` + `crop` llevan esa ventana al tamano exacto del panel sin
///     deformarla (ver `cover`).
///  3. `zoompan` solo hace el punch-in, sobre una imagen que ya tiene la
///     proporcion correcta.
///
/// Meter el fotograma entero en zoompan y dejar que encaje al panel aplasta la
/// imagen: zoompan recorta siempre con la proporcion de la ENTRADA y luego la
/// escala a la de SALIDA.
fn reframe_filters(
    source: &MediaInfo,
    target_width: i64,
    target_height: i64,
    track: Option<&Track>,
    plan: &JumpCutPlan,
    cfg: &Config,
    options: GraphOptions,
) -> Vec<String> {
    let (crop_w, crop_h) = fit_crop(source, target_width, target_height);
    let centre_y = source.height as f64 * 0.45; // las caras caen algo por encima del centro
    let centre_x = centre_expression(track, plan, source.display_width() as f64, "t");

    let x = format!("clip({centre_x}-{:.1},0,in_w-{crop_w})", crop_w as f64 / 2.0);
    let y = format!("clip({centre_y:.1}-{:.1},0,in_h-{crop_h})", crop_h as f64 / 2.0);
    let evaluation = if options.crop_has_eval { ":eval=frame" } else { "" };

    let mut filters = vec![format!("crop=w={crop_w}:h={crop_h}:x='{x}':y='{y}'{evaluation}")];
    filters.extend(cover(target_width, target_height));
    if !cfg.zoom.enabled {
        return filters;
    }

    // zoompan escala la entrada por z y extrae una ventana de salida fija. Aqui
    // entrada y salida tienen el mismo tamano, asi que z es literalmente el
    // punch-in. Dentro de zoompan NO existe la variable `t`: el tiempo sale del
    // contador de fotogramas `on`. Y z se recorta a un minimo de 1: por debajo,
    // zoompan lo ignora.
    let clock = format!("(on/{})", cfg.layout.fps);
    let low = cfg.zoom.min_zoom.max(1.0);
    let high = cfg.zoom.max_zoom.max(low);
    let punch = format!(
        "({}+{}*(0.5-0.5*cos(2*PI*{clock}/{})))",
        fl(low),
        fl(high - low),
        fl(cfg.zoom.period)
    );
    filters.push(format!(
        "zoompan=z='{punch}':x='(iw-iw/zoom)/2':y='(ih-ih/zoom)/2':d=1:s={target_width}x{target_height}:fps={}",
        cfg.layout.fps
    ));
    filters
}

/// Convierte a pixeles cuadrados si el origen no los tiene. Sin esto, un video
/// anamorfico se recorta con una geometria que no es la que se ve.
fn square_pixels(source: &MediaInfo) -> Vec<String> {
    if source.square_pixels() {
        return Vec::new();
    }
    vec![format!("scale={}:{}:flags=bicubic", source.display_width(), source.height), "setsar=1".into()]
}

/// Lleva la imagen al tamano exacto del panel SIN deformarla.
/// `force_original_aspect_ratio=increase` escala hasta cubrir el panel
/// manteniendo la proporcion, y el `crop` se come el sobrante. Un `scale=W:H`
/// a secas estiraria la imagen.
fn cover(target_width: i64, target_height: i64) -> Vec<String> {
    vec![
        format!("scale={target_width}:{target_height}:force_original_aspect_ratio=increase:flags=bicubic"),
        format!("crop={target_width}:{target_height}"),
    ]
}

fn center_crop(source: &MediaInfo, target_width: i64, target_height: i64) -> String {
    let (crop_w, crop_h) = fit_crop(source, target_width, target_height);
    format!("crop=w={crop_w}:h={crop_h}:x=(in_w-{crop_w})/2:y=(in_h-{crop_h})/2")
}

/// Mayor recorte del origen que tiene la proporcion del panel destino. Se mide
/// sobre la anchura de VISUALIZACION (pixeles cuadrados).
pub fn fit_crop(source: &MediaInfo, target_width: i64, target_height: i64) -> (i64, i64) {
    let source_width = source.display_width();
    let target_aspect = target_width as f64 / target_height as f64;
    let (crop_w, crop_h) = if source.aspect() > target_aspect {
        let crop_h = source.height;
        (((crop_h as f64) * target_aspect).round_ties_even() as i64, crop_h)
    } else {
        let crop_w = source_width;
        (crop_w, ((crop_w as f64) / target_aspect).round_ties_even() as i64)
    };
    (even(crop_w.min(source_width)), even(crop_h.min(source.height)))
}

/// Expresion de ffmpeg con la x del centro del encuadre, en pixeles.
pub fn centre_expression(track: Option<&Track>, plan: &JumpCutPlan, source_width: f64, time_var: &str) -> String {
    let Some(track) = track.filter(|t| !t.points.is_empty()) else {
        return format!("{:.1}", source_width / 2.0);
    };

    // La trayectoria se midio sobre el clip SIN cortar; el render la ve ya
    // recortada, asi que hay que trasladar cada instante.
    let mut points: Vec<(f64, f64)> = Vec::new();
    for &(t, cx) in &track.points {
        let moment = plan.remap(t);
        if points.last().is_some_and(|last| moment <= last.0 + 1e-3) {
            continue; // cayo dentro de un hueco eliminado
        }
        points.push((moment, cx * source_width));
    }
    piecewise_linear(&points, time_var)
}

/// Interpolacion lineal por tramos como expresion de ffmpeg.
pub fn piecewise_linear(points: &[(f64, f64)], time_var: &str) -> String {
    match points {
        [] => return "0".into(),
        [only] => return format!("{:.1}", only.1),
        _ => {}
    }

    let mut expression = format!("{:.1}", points[points.len() - 1].1); // despues del ultimo punto: fijo
    for pair in points.windows(2).rev() {
        let ((t0, x0), (t1, x1)) = (pair[0], pair[1]);
        let span = (t1 - t0).max(1e-6);
        let slope = (x1 - x0) / span;
        let segment = format!("({x0:.1}+{slope:.4}*({time_var}-{t0:.3}))");
        expression = format!("if(lt({time_var},{t1:.3}),{segment},{expression})");
    }

    // Antes del primer punto se mantiene el valor inicial.
    format!("if(lt({time_var},{:.3}),{:.1},{expression})", points[0].0, points[0].1)
}

/// `between(...)+between(...)`: en ffmpeg el + actua como OR logico.
pub fn select_expr(keeps: &[(f64, f64)]) -> String {
    keeps.iter().map(|(a, b)| format!("between(t,{a:.3},{b:.3})")).collect::<Vec<_>>().join("+")
}

// ----------------------------------------------------------------------------
// Codificador
// ----------------------------------------------------------------------------

/// Argumentos del codificador. Se usa la GPU solo si de verdad la hay.
pub fn video_encoder_args(out: &OutputConfig, encoder: &str) -> Vec<String> {
    let crf = out.crf.to_string();
    let args = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();

    match encoder {
        "h264_nvenc" => args(&["-c:v", encoder, "-preset", "p5", "-rc", "vbr", "-cq", &crf, "-b:v", "0"]),
        "h264_qsv" => args(&["-c:v", encoder, "-preset", "medium", "-global_quality", &crf]),
        "h264_amf" => args(&[
            "-c:v", encoder, "-quality", "balanced", "-rc", "cqp",
            "-qp_i", &crf, "-qp_p", &crf, "-qp_b", &crf,
        ]),
        // Un codificador que ha pedido el usuario y no conocemos: se pasa tal
        // cual con una calidad razonable.
        other if !other.is_empty() && other != out.video_codec => args(&["-c:v", other, "-b:v", "6M"]),
        _ => args(&["-c:v", &out.video_codec, "-preset", &out.preset, "-crf", &crf]),
    }
}

/// Nombre del codificador a usar. Se decide una vez por ejecucion.
pub fn resolve_encoder(out: &OutputConfig) -> String {
    static CHOICE: OnceLock<String> = OnceLock::new();
    CHOICE
        .get_or_init(|| {
            let preference = if out.use_nvenc { "h264_nvenc" } else { out.hardware_encoder.as_str() };
            let chosen = ffmpeg::detect_hardware_encoder(preference).unwrap_or_else(|| out.video_codec.clone());
            if chosen == out.video_codec {
                log::info!("Codificando por CPU con {} (preset {}).", out.video_codec, out.preset);
            }
            chosen
        })
        .clone()
}

/// Guarda un fotograma como portada, junto al short. Si falla no pasa nada: el
/// short ya esta hecho.
pub fn extract_thumbnail(video: &Path, at: f64) -> Option<PathBuf> {
    let output = video.with_extension("jpg");
    let args: Vec<String> = [
        "-ss".to_string(), format!("{:.3}", at.max(0.0)),
        "-i".to_string(), abs(video),
        "-frames:v".to_string(), "1".to_string(),
        "-q:v".to_string(), "3".to_string(),
        abs(&output),
    ]
    .to_vec();
    let name = video.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    if let Err(e) = ffmpeg::run(&args, None, None, &format!("portada {name}")) {
        log::debug!("No se pudo sacar la portada de {name}: {e}");
        return None;
    }
    output.is_file().then_some(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn video(width: i64, height: i64, sar: f64) -> MediaInfo {
        MediaInfo { sar, ..MediaInfo::new("v.mp4", width, height) }
    }

    fn peticion(plan: Option<JumpCutPlan>, track: Option<Track>, ass: Option<&str>) -> RenderRequest {
        RenderRequest {
            podcast: "p.mp4".into(),
            background: "b.mp4".into(),
            start: 10.0,
            end: 40.0,
            background_offset: 0.0,
            plan: plan.unwrap_or_else(|| JumpCutPlan::full(30.0)),
            track,
            ass_name: ass.map(String::from),
            output: "short_1.mp4".into(),
            work_dir: "w".into(),
        }
    }

    fn grafo_con(cfg: &Config, plan: Option<JumpCutPlan>, track: Option<Track>, ass: Option<&str>, eval: bool) -> String {
        let arriba = even((cfg.layout.height as f64 * cfg.layout.top_ratio) as i64);
        build_graph(
            &peticion(plan, track, ass),
            &video(1920, 1080, 1.0),
            &video(1920, 1080, 1.0),
            arriba,
            cfg.layout.height - arriba,
            cfg,
            GraphOptions { crop_has_eval: eval },
        )
    }

    fn grafo() -> String {
        grafo_con(&Config::default(), None, None, Some("short_1.ass"), false)
    }

    // -- geometria -----------------------------------------------------------

    #[test]
    fn el_recorte_tiene_la_proporcion_del_panel() {
        let origenes = [video(1920, 1080, 1.0), video(1280, 720, 1.0), video(1080, 1920, 1.0),
                        video(3840, 2160, 1.0), video(720, 576, 16.0 / 11.0)];
        for origen in &origenes {
            for (ancho, alto) in [(1080, 960), (1080, 1280), (1080, 640)] {
                let (w, h) = fit_crop(origen, ancho, alto);
                let medida = (origen.width, origen.height, ancho, alto);
                assert!((w as f64 / h as f64 - ancho as f64 / alto as f64).abs() < 0.01, "{medida:?}");
                assert!(w <= origen.display_width() && h <= origen.height, "{medida:?}");
                assert_eq!((w % 2, h % 2), (0, 0), "{medida:?}");
            }
        }
    }

    #[test]
    fn el_recorte_de_un_16_9_para_un_panel_cuadradito() {
        assert_eq!(fit_crop(&video(1920, 1080, 1.0), 1080, 960), (1214, 1080));
    }

    #[test]
    fn pixeles_no_cuadrados_se_corrigen_antes_de_recortar() {
        assert!(!square_pixels(&video(720, 576, 16.0 / 11.0)).is_empty());
        assert!(square_pixels(&video(1920, 1080, 1.0)).is_empty());
    }

    #[test]
    fn cover_no_estira() {
        let filtros = cover(1080, 960);
        assert!(filtros[0].contains("force_original_aspect_ratio=increase"));
        assert_eq!(filtros[1], "crop=1080:960");
    }

    // -- estructura ----------------------------------------------------------

    #[test]
    fn las_etiquetas_que_ffmpeg_mapea_existen() {
        let g = grafo();
        for etiqueta in ["[top]", "[bottom]", "[vout]", "[aout]", "vstack=inputs=2"] {
            assert!(g.contains(etiqueta), "{etiqueta}");
        }
        assert_eq!(g.matches("[top]").count(), 2);
        assert_eq!(g.matches("[bottom]").count(), 2);
    }

    #[test]
    fn subtitulos_solo_si_hay_fichero_ass() {
        assert!(grafo().contains("subtitles=filename=short_1.ass"));
        assert!(!grafo_con(&Config::default(), None, None, None, false).contains("subtitles="));
    }

    #[test]
    fn carpeta_de_fuentes_propia() {
        let mut cfg = Config::default();
        cfg.subtitles.fonts_dir = Some("fuentes".into());
        assert!(grafo_con(&cfg, None, None, Some("a.ass"), false).contains(":fontsdir="));
    }

    #[test]
    fn normalizacion_de_volumen() {
        let mut cfg = Config::default();
        assert!(grafo_con(&cfg, None, None, None, false).contains("loudnorm=I=-14.0:TP=-1.5:LRA=11.0"));
        cfg.audio.loudnorm = false;
        assert!(!grafo_con(&cfg, None, None, None, false).contains("loudnorm"));
    }

    #[test]
    fn zoom_se_puede_apagar_y_mantiene_el_tamano_del_panel() {
        let mut cfg = Config::default();
        let con = grafo_con(&cfg, None, None, None, false);
        assert!(con.contains("zoompan=") && con.contains("s=1080x960"));
        cfg.zoom.enabled = false;
        assert!(!grafo_con(&cfg, None, None, None, false).contains("zoompan"));
    }

    #[test]
    fn la_expresion_del_zoom_no_pierde_el_decimal() {
        let g = grafo();
        assert!(g.contains("(1.0+0.07"), "{g}");
        assert!(g.contains("/14.0)))"), "{g}");
    }

    #[test]
    fn eval_frame_solo_si_este_ffmpeg_lo_acepta() {
        let cfg = Config::default();
        assert!(grafo_con(&cfg, None, None, None, true).contains(":eval=frame"));
        assert!(!grafo_con(&cfg, None, None, None, false).contains(":eval=frame"));
    }

    #[test]
    fn el_audio_sale_en_stereo_a_48k() {
        let g = grafo();
        assert!(g.contains("aresample=48000") && g.contains("channel_layouts=stereo"));
    }

    #[test]
    fn el_fondo_se_recorta_centrado_al_panel_inferior() {
        let g = grafo();
        let bottom = g.split(";\n").find(|c| c.ends_with("[bottom]")).unwrap();
        assert!(bottom.contains("(in_w-") && bottom.contains("crop=1080:960"));
    }

    #[test]
    fn con_cortes_video_y_audio_usan_la_misma_seleccion() {
        let plan = JumpCutPlan { keeps: vec![(0.0, 5.0), (7.0, 12.0)], source_duration: 12.0 };
        let g = grafo_con(&Config::default(), Some(plan.clone()), None, None, false);
        let expresion = select_expr(&plan.keeps);
        assert_eq!(expresion, "between(t,0.000,5.000)+between(t,7.000,12.000)");
        assert!(g.contains(&format!("select='{expresion}'")));
        assert!(g.contains(&format!("aselect='{expresion}'")));
        assert!(!grafo().contains("select="));
    }

    #[test]
    fn select_usa_suma_como_or() {
        let e = select_expr(&[(0.0, 1.0), (3.0, 4.0)]);
        assert_eq!(e.matches("between").count(), 2);
        assert!(e.contains('+'));
    }

    // -- expresiones ---------------------------------------------------------

    /// Evaluador minimo de las expresiones de ffmpeg que generamos: numeros,
    /// `t`, + - * /, parentesis, `if(c,a,b)` y `lt(a,b)`.
    struct Ev<'a> { s: &'a [u8], i: usize, t: f64 }

    impl Ev<'_> {
        fn peek(&self) -> Option<u8> { self.s.get(self.i).copied() }
        fn expr(&mut self) -> f64 {
            let mut v = self.term();
            while let Some(op @ (b'+' | b'-')) = self.peek() {
                self.i += 1;
                let r = self.term();
                v = if op == b'+' { v + r } else { v - r };
            }
            v
        }
        fn term(&mut self) -> f64 {
            let mut v = self.factor();
            while let Some(op @ (b'*' | b'/')) = self.peek() {
                self.i += 1;
                let r = self.factor();
                v = if op == b'*' { v * r } else { v / r };
            }
            v
        }
        fn factor(&mut self) -> f64 {
            match self.peek().expect("expresion cortada") {
                b'-' => { self.i += 1; -self.factor() }
                b'(' => { self.i += 1; let v = self.expr(); self.i += 1; v }
                c if c.is_ascii_digit() || c == b'.' => {
                    let start = self.i;
                    while let Some(c) = self.peek() {
                        let exp_sign = matches!(c, b'+' | b'-') && matches!(self.s[self.i - 1], b'e' | b'E');
                        if c.is_ascii_digit() || c == b'.' || c == b'e' || c == b'E' || exp_sign { self.i += 1 } else { break }
                    }
                    std::str::from_utf8(&self.s[start..self.i]).unwrap().parse().unwrap()
                }
                _ => {
                    let start = self.i;
                    while self.peek().is_some_and(|c| c.is_ascii_alphabetic()) { self.i += 1; }
                    let name = std::str::from_utf8(&self.s[start..self.i]).unwrap().to_string();
                    if self.peek() != Some(b'(') {
                        assert_eq!(name, "t", "variable desconocida {name}");
                        return self.t;
                    }
                    self.i += 1;
                    let mut args = vec![self.expr()];
                    while self.peek() == Some(b',') { self.i += 1; args.push(self.expr()); }
                    self.i += 1;
                    match name.as_str() {
                        "if" => if args[0] != 0.0 { args[1] } else { args[2] },
                        "lt" => (args[0] < args[1]) as i32 as f64,
                        other => panic!("funcion desconocida {other}"),
                    }
                }
            }
        }
    }

    fn evaluar(expresion: &str, t: f64) -> f64 {
        Ev { s: expresion.as_bytes(), i: 0, t }.expr()
    }

    #[test]
    fn el_evaluador_de_pruebas_sabe_lo_que_hace() {
        assert_eq!(evaluar("if(lt(t,5),1+2*3,-4)", 1.0), 7.0);
        assert_eq!(evaluar("if(lt(t,5),1+2*3,-4)", 9.0), -4.0);
        assert_eq!(evaluar("(10.0+-2.5000*(t-1.000))", 3.0), 5.0);
    }

    #[test]
    fn interpolacion_por_tramos_devuelve_los_extremos() {
        let e = piecewise_linear(&[(0.0, 100.0), (2.0, 300.0)], "t");
        assert!(e.contains("100.0") && e.contains("300.0") && e.contains("if(lt(t,"));
    }

    #[test]
    fn un_solo_punto_es_una_constante() {
        assert_eq!(piecewise_linear(&[(0.0, 540.0)], "t"), "540.0");
        assert_eq!(piecewise_linear(&[], "t"), "0");
    }

    #[test]
    fn interpolacion_lineal_entre_puntos() {
        let e = piecewise_linear(&[(0.0, 100.0), (10.0, 300.0), (20.0, 300.0), (30.0, 100.0)], "t");
        for (t, esperado) in [(-5.0, 100.0), (0.0, 100.0), (5.0, 200.0), (10.0, 300.0), (15.0, 300.0),
                              (25.0, 200.0), (30.0, 100.0), (99.0, 100.0)] {
            assert!((evaluar(&e, t) - esperado).abs() < 0.5, "t={t}");
        }
    }

    #[test]
    fn sin_trayectoria_el_centro_es_el_del_video() {
        assert_eq!(centre_expression(None, &JumpCutPlan::full(30.0), 1920.0, "t"), "960.0");
        let vacia = Track { points: vec![], detection_ratio: 0.0 };
        assert_eq!(centre_expression(Some(&vacia), &JumpCutPlan::full(30.0), 1920.0, "t"), "960.0");
    }

    #[test]
    fn la_trayectoria_sigue_a_la_imagen_tras_quitar_silencios() {
        // La cara pasa de la izquierda (0.25) a la derecha (0.75) en el segundo
        // 8, que cae en el hueco eliminado [5, 7]: tras el corte esos dos
        // segundos no existen y el 8 original es el 6 del video.
        let plan = JumpCutPlan { keeps: vec![(0.0, 5.0), (7.0, 12.0)], source_duration: 12.0 };
        let pista = Track { points: vec![(0.0, 0.25), (8.0, 0.25), (9.0, 0.75), (12.0, 0.75)], detection_ratio: 1.0 };
        let e = centre_expression(Some(&pista), &plan, 1000.0, "t");
        for (t, esperado) in [(0.0, 250.0), (5.9, 250.0), (7.0, 750.0), (10.0, 750.0)] {
            assert!((evaluar(&e, t) - esperado).abs() < 1.0, "t={t}");
        }
    }

    #[test]
    fn el_recorte_horizontal_nunca_se_sale_del_video() {
        let pista = Track { points: vec![(0.0, 0.0), (10.0, 1.0)], detection_ratio: 1.0 };
        let g = grafo_con(&Config::default(), None, Some(pista), None, false);
        assert!(g.contains("clip(") && g.contains(",0,in_w-1214)"), "{g}");
    }

    // -- codificador ---------------------------------------------------------

    #[test]
    fn codificadores() {
        let out = OutputConfig::default();
        let cpu = video_encoder_args(&out, "libx264");
        assert_eq!(cpu[..2], ["-c:v", "libx264"]);
        assert!(cpu.contains(&"-crf".to_string()));

        let nvenc = video_encoder_args(&out, "h264_nvenc");
        assert!(nvenc.contains(&"-cq".to_string()) && !nvenc.contains(&"-crf".to_string()));

        assert_eq!(video_encoder_args(&out, "h264_mf")[..2], ["-c:v", "h264_mf"]);
        assert!(video_encoder_args(&out, "h264_qsv").contains(&"-global_quality".to_string()));
        assert!(video_encoder_args(&out, "h264_amf").contains(&"-qp_i".to_string()));
        assert_eq!(video_encoder_args(&out, "")[..2], ["-c:v", "libx264"]);
    }
}
