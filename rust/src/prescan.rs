//! Preseleccion de zonas calientes ANTES de transcribir.
//!
//! Transcribir un podcast de tres horas cuesta unos cien minutos de CPU, y el
//! 98% de ese trabajo se tira: al final solo se usan tres tramos de un minuto.
//!
//! Las dos senales mas fuertes del pipeline -- el heatmap de YouTube y la
//! energia del audio -- no necesitan texto y se calculan en segundos sobre el
//! video entero. Este modulo las usa para decidir DONDE mirar, de modo que
//! Whisper solo transcriba esas zonas.

use crate::audio_energy::EnergyTrack;
use crate::config::{ClipsConfig, PrescanConfig, ScoringConfig};
use crate::download::HeatPoint;

pub type Region = (f64, f64);

/// Devuelve tramos [(inicio, fin)] ordenados y sin solaparse.
pub fn choose_regions(
    duration: f64,
    heatmap: &[HeatPoint],
    energy: Option<&EnergyTrack>,
    prescan: &PrescanConfig,
    clips: &ClipsConfig,
    scoring: &ScoringConfig,
) -> Vec<Region> {
    let mut usable_start = clips.head_skip;
    let mut usable_end = duration - clips.tail_skip;
    if usable_end - usable_start < clips.min_duration {
        usable_start = 0.0;
        usable_end = duration;
    }

    if heatmap.is_empty() && energy.is_none() {
        log::info!("Sin heatmap ni energia: se transcribe el video entero.");
        return vec![(usable_start, usable_end)];
    }

    let span = clips.max_duration;
    let step = prescan.step.max(1.0);

    // 1. Barrido grueso: se puntua cada ventana posible con las senales baratas.
    let mut scored: Vec<(f64, f64, f64, f64)> = Vec::new();
    let mut start = usable_start;
    while start + span <= usable_end {
        let end = start + span;
        let heat = heat_for(heatmap, start, end);
        let level = energy.map_or(0.0, |e| e.window_score(start, end));
        scored.push((start, end, heat, level));
        start += step;
    }
    if scored.is_empty() {
        return vec![(usable_start, usable_end)];
    }

    let fold = |f: fn(f64, f64) -> f64, init: f64, pick: fn(&(f64, f64, f64, f64)) -> f64| {
        scored.iter().map(pick).fold(init, f)
    };
    let heat_range = (fold(f64::min, f64::INFINITY, |r| r.2), fold(f64::max, f64::NEG_INFINITY, |r| r.2));
    let energy_range = (fold(f64::min, f64::INFINITY, |r| r.3), fold(f64::max, f64::NEG_INFINITY, |r| r.3));
    let combined = |row: &(f64, f64, f64, f64)| {
        combine(row.2, row.3, (scoring.weight_heatmap, scoring.weight_energy), (heat_range, energy_range))
    };
    let mut ranked = scored.clone();
    ranked.sort_by(|a, b| combined(b).total_cmp(&combined(a)));

    // 2. Las mejores sin solaparse, con hueco suficiente para que luego puedan
    //    salir clips distintos.
    let wanted = if prescan.regions > 0 { prescan.regions } else { (clips.num_clips * 5).max(12) } as usize;
    let mut picked: Vec<Region> = Vec::new();
    for &(win_start, win_end, _, _) in &ranked {
        if picked.len() >= wanted {
            break;
        }
        if picked.iter().any(|&(p_start, p_end)| win_start < p_end && win_end > p_start) {
            continue;
        }
        picked.push((win_start, win_end));
    }

    // 3. Margen a cada lado y fusion de los que se toquen.
    let pad = prescan.padding;
    let mut padded: Vec<Region> =
        picked.iter().map(|&(s, e)| ((s - pad).max(usable_start), (e + pad).min(usable_end))).collect();
    padded.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mut regions: Vec<Region> = Vec::new();
    for (region_start, region_end) in padded {
        match regions.last_mut() {
            Some(last) if region_start <= last.1 => last.1 = last.1.max(region_end),
            _ => regions.push((region_start, region_end)),
        }
    }

    let covered: f64 = regions.iter().map(|(s, e)| e - s).sum();
    log::info!(
        "Preseleccion: {} zonas, {} de audio a transcribir de {} ({:.0}% del total).",
        regions.len(),
        fmt(covered),
        fmt(duration),
        100.0 * covered / duration.max(1e-6)
    );
    for &(region_start, region_end) in &regions {
        log::debug!("  zona {}-{}", fmt(region_start), fmt(region_end));
    }
    regions
}

/// Suma ponderada con cada senal normalizada 0..1 sobre la poblacion. Una
/// senal plana (sin heatmap, por ejemplo) no aporta informacion, asi que se
/// ignora y su peso se reparte entre las demas.
fn combine(heat: f64, level: f64, weights: (f64, f64), ranges: ((f64, f64), (f64, f64))) -> f64 {
    let (mut total, mut total_weight) = (0.0, 0.0);
    for (value, weight, (low, high)) in [(heat, weights.0, ranges.0), (level, weights.1, ranges.1)] {
        if weight <= 0.0 || high - low < 1e-9 {
            continue;
        }
        total += weight * (value - low) / (high - low);
        total_weight += weight;
    }
    if total_weight > 0.0 { total / total_weight } else { 0.0 }
}

/// Media del heatmap en [start, end), ponderada por solape temporal.
pub fn heat_for(heatmap: &[HeatPoint], start: f64, end: f64) -> f64 {
    let (mut accumulated, mut total_weight) = (0.0, 0.0);
    for point in heatmap {
        let overlap = end.min(point.end) - start.max(point.start);
        if overlap <= 0.0 {
            continue;
        }
        accumulated += point.value * overlap;
        total_weight += overlap;
    }
    if total_weight > 0.0 { accumulated / total_weight } else { 0.0 }
}

fn fmt(seconds: f64) -> String {
    let total = seconds as i64;
    let (minutes, secs) = (total / 60, total % 60);
    let (hours, minutes) = (minutes / 60, minutes % 60);
    if hours > 0 { format!("{hours}h{minutes:02}m") } else { format!("{minutes}m{secs:02}s") }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn heat(spans: &[(f64, f64, f64)]) -> Vec<HeatPoint> {
        spans.iter().map(|&(start, end, value)| HeatPoint { start, end, value }).collect()
    }

    fn configs() -> (PrescanConfig, ClipsConfig, ScoringConfig) {
        let clips = ClipsConfig { min_duration: 25.0, max_duration: 45.0, head_skip: 60.0, tail_skip: 30.0, ..Default::default() };
        (PrescanConfig::default(), clips, ScoringConfig::default())
    }

    #[test]
    fn sin_senales_se_transcribe_todo_lo_util() {
        let (p, c, s) = configs();
        assert_eq!(choose_regions(3600.0, &[], None, &p, &c, &s), vec![(60.0, 3570.0)]);
    }

    #[test]
    fn un_video_mas_corto_que_los_margenes_ignora_los_margenes() {
        let (p, c, s) = configs();
        assert_eq!(choose_regions(70.0, &[], None, &p, &c, &s), vec![(0.0, 70.0)]);
    }

    #[test]
    fn la_zona_caliente_del_heatmap_se_elige_con_su_margen() {
        let (mut p, c, s) = configs();
        p.regions = 1;
        // Un video de 20 minutos, plano salvo el tramo 600-660.
        let mapa = heat(&[(0.0, 600.0, 0.1), (600.0, 660.0, 1.0), (660.0, 1200.0, 0.1)]);
        let zonas = choose_regions(1200.0, &mapa, None, &p, &c, &s);
        assert_eq!(zonas.len(), 1);
        let (inicio, fin) = zonas[0];
        assert!(inicio <= 600.0 && fin >= 660.0, "{inicio}-{fin} no cubre el tramo caliente");
        assert!(fin - inicio <= 45.0 + 2.0 * p.padding + 1e-9);
    }

    #[test]
    fn las_zonas_salen_ordenadas_sin_solaparse_y_dentro_de_lo_util() {
        let (p, c, s) = configs();
        let mut puntos = Vec::new();
        for i in 0..120 {
            puntos.push((i as f64 * 10.0, i as f64 * 10.0 + 10.0, ((i * 37) % 11) as f64 / 10.0));
        }
        let zonas = choose_regions(1200.0, &heat(&puntos), None, &p, &c, &s);
        assert!(!zonas.is_empty());
        for w in zonas.windows(2) {
            assert!(w[0].1 < w[1].0, "{:?} y {:?} se solapan", w[0], w[1]);
        }
        for &(a, b) in &zonas {
            assert!(a >= 60.0 && b <= 1170.0 && a < b);
        }
    }

    #[test]
    fn la_energia_sola_tambien_decide() {
        let (mut p, c, s) = configs();
        p.regions = 1;
        let mut level = vec![0.1f32; 4800]; // 20 minutos
        for l in &mut level[2000..2200] {
            *l = 1.0; // 500-550 s
        }
        let energia = EnergyTrack::new(level, 0.25, 1200.0, 0.0);
        let zonas = choose_regions(1200.0, &[], Some(&energia), &p, &c, &s);
        assert_eq!(zonas.len(), 1);
        assert!(zonas[0].0 <= 520.0 && zonas[0].1 >= 530.0, "{zonas:?}");
    }

    #[test]
    fn una_senal_plana_no_cuenta() {
        // Sin heatmap util, manda la energia: el peso del heatmap no puede diluirla.
        assert_eq!(combine(0.5, 1.0, (0.3, 0.15), ((0.5, 0.5), (0.0, 1.0))), 1.0);
        assert_eq!(combine(0.5, 0.5, (0.3, 0.15), ((0.5, 0.5), (0.5, 0.5))), 0.0);
    }

    #[test]
    fn media_del_heatmap_ponderada_por_solape() {
        let mapa = heat(&[(0.0, 10.0, 1.0), (10.0, 40.0, 0.0)]);
        assert!((heat_for(&mapa, 0.0, 20.0) - 0.5).abs() < 1e-12);
        assert_eq!(heat_for(&mapa, 100.0, 120.0), 0.0);
        assert_eq!(heat_for(&[], 0.0, 1.0), 0.0);
    }

    #[test]
    fn formato_de_tiempo() {
        assert_eq!(fmt(75.0), "1m15s");
        assert_eq!(fmt(3725.0), "1h02m");
    }
}
