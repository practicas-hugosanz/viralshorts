//! viralshorts: de un podcast de YouTube a shorts verticales subtitulados.
//!
//! Sin argumentos abre la aplicacion (una ventana con la interfaz). Con
//! `--podcast` y `--background` (o `--from-plan`) elige los momentos y monta los
//! shorts por linea de comandos. Los subcomandos son utilidades:
//!
//!   viralshorts doctor    comprueba ffmpeg, whisper.cpp, el codificador y la configuracion
//!   viralshorts config    muestra la configuracion efectiva (YAML + --set)
//!   viralshorts gui       la interfaz en el navegador, sin ventana propia
//!
//! En Windows es una aplicacion de ventana (sin consola al hacer doble clic).
//! Lanzada desde una terminal se engancha a ella para poder escribir.

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use std::path::Path;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use viralshorts::config::{Config, ConfigError};
use viralshorts::pipeline::{self, Args};
use viralshorts::{ffmpeg, logging};

#[derive(Parser)]
#[command(name = "viralshorts", version, about = "De un podcast de YouTube a shorts verticales subtitulados.")]
struct Cli {
    #[command(flatten)]
    args: Args,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Comprueba ffmpeg, whisper.cpp, el codificador y la configuracion.
    Doctor,
    /// Muestra la configuracion efectiva (YAML + --set) como YAML.
    Config,
    /// Abre la interfaz visual en el navegador.
    Gui {
        /// Puerto preferido (si esta ocupado se prueba el siguiente).
        #[arg(long, default_value_t = viralshorts::gui::DEFAULT_PORT)]
        port: u16,
        /// No abrir el navegador automaticamente.
        #[arg(long)]
        no_browser: bool,
    },
}

/// Sin consola propia, `println!` y los registros no tienen donde escribir
/// cuando se lanza desde una terminal: se engancha a la de quien lo lanzo.
#[cfg(windows)]
fn attach_parent_console() {
    // ATTACH_PARENT_PROCESS = (DWORD)-1. Si no hay terminal (doble clic) falla y no pasa nada.
    unsafe {
        windows_sys::Win32::System::Console::AttachConsole(u32::MAX);
    }
}

#[cfg(not(windows))]
fn attach_parent_console() {}

fn main() -> ExitCode {
    attach_parent_console();
    let cli = Cli::parse();
    logging::setup(cli.args.verbose);
    ffmpeg::use_portable_tools(&ffmpeg::project_root());

    let sin_trabajo = cli.args.podcast.is_none() && cli.args.background.is_none() && cli.args.from_plan.is_none() && cli.args.remake.is_none();
    let code = match &cli.command {
        None if sin_trabajo => viralshorts::app::run(),
        None => pipeline::run(&cli.args),
        Some(Command::Config) => show_config(&cli.args),
        Some(Command::Doctor) => doctor(&cli.args),
        Some(Command::Gui { port, no_browser }) => viralshorts::gui::run(*port, !*no_browser),
    };
    ExitCode::from(code as u8)
}

fn load(args: &Args) -> Result<Config, ConfigError> {
    let path = pipeline::resolve_config(&args.config);
    let path: &Path = &path;
    let mut cfg = Config::load(path.is_file().then_some(path))?;
    for item in &args.overrides {
        cfg.set(item)?;
    }
    Ok(cfg)
}

fn show_config(args: &Args) -> i32 {
    match load(args).map_err(|e| e.to_string()).and_then(|cfg| serde_yaml_ng::to_string(&cfg).map_err(|e| e.to_string())) {
        Ok(yaml) => {
            print!("{yaml}");
            0
        }
        Err(error) => {
            log::error!("{error}");
            2
        }
    }
}

fn doctor(args: &Args) -> i32 {
    let mut problems = 0;

    println!("Entorno");
    for tool in ["ffmpeg", "ffprobe"] {
        match ffmpeg::which(tool) {
            Some(path) => println!("  OK    {tool}: {}", path.display()),
            None => {
                println!("  FALTA {tool} no esta en el PATH (doble clic en viralshorts.bat lo instala)");
                problems += 1;
            }
        }
    }
    match ffmpeg::which("yt-dlp") {
        Some(path) => println!("  OK    yt-dlp: {}", path.display()),
        None => println!("  --    yt-dlp: no esta instalado (hace falta para descargar de YouTube)"),
    }
    match viralshorts::transcribe::WhisperCpp::discover(&ffmpeg::project_root()) {
        Ok(_) => println!("  OK    whisper.cpp (whisper-cli)"),
        Err(_) => {
            println!("  FALTA whisper-cli (whisper.cpp) en bin/whisper o en el PATH");
            problems += 1;
        }
    }

    // La tarjeta grafica acelera la transcripcion con su motor: CUDA (NVIDIA) o Vulkan (AMD, Intel).
    let root = ffmpeg::project_root();
    match viralshorts::setup::gpu_engine_for_this_pc() {
        Some(_) if viralshorts::setup::gpu_engine_installed(&root) => println!("  OK    tarjeta grafica: la transcripcion la usara"),
        Some(tool) => println!("  --    tarjeta grafica sin su motor ({}): se transcribe con el procesador; se activa desde la app", tool.label()),
        None => println!("  --    sin tarjeta grafica aprovechable: se transcribe con el procesador (funciona igual, tarda mas)"),
    }

    if ffmpeg::which("ffmpeg").is_some() {
        let preference = load(args).map(|c| c.output.hardware_encoder).unwrap_or_else(|_| "auto".into());
        match ffmpeg::detect_hardware_encoder(&preference) {
            Some(name) => println!("  OK    codificador por hardware: {name}"),
            None => println!("  OK    codificador: CPU (libx264)"),
        }
    }

    println!("Configuracion");
    match load(args) {
        Ok(cfg) => {
            println!(
                "  OK    {} ({} clips de {}-{}s, modelo {})",
                if args.config.is_file() { args.config.display().to_string() } else { "valores por defecto".into() },
                cfg.clips.num_clips,
                cfg.clips.min_duration,
                cfg.clips.max_duration,
                cfg.llm.model
            );
            let key = std::env::var_os("ANTHROPIC_API_KEY").is_some() || std::env::var_os("ANTHROPIC_AUTH_TOKEN").is_some();
            println!("  {}    clave de Claude{}", if key { "OK  " } else { "--  " }, if key { "" } else { ": no hay; se puntuara sin Claude" });
        }
        Err(error) => {
            println!("  FALTA {error}");
            problems += 1;
        }
    }

    i32::from(problems > 0)
}
