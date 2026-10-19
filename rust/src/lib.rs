//! viralshorts: de un podcast de YouTube a shorts verticales subtitulados.
//!
//! Migracion a Rust de la version Python. Cada modulo hace lo mismo que su
//! homonimo de `viralshorts/*.py`.

pub mod app;
pub mod audio_energy;
pub mod config;
pub mod context;
pub mod download;
pub mod exports;
pub mod facecrop;
pub mod fetch;
pub mod ffmpeg;
pub mod fonts;
pub mod gui;
pub mod jumpcuts;
pub mod llm;
pub mod logging;
pub mod metadata;
pub mod pipeline;
pub mod plan;
pub mod prescan;
pub mod progress;
pub mod render;
pub mod scoring;
pub mod setup;
pub mod subtitles;
pub mod transcribe;
pub mod update;
pub mod words;
pub mod worklock;
