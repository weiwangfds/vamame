//! Automatic measurement calculations for oscilloscope channels.
//!
//! Computes voltage statistics via Polars aggregations (fast on any data size)
//! and time-domain measurements (frequency, rise/fall time, duty cycle) from
//! raw sample points.

use crate::data::WaveformData;

/// All automatic measurements for one channel in the visible range.
#[derive(Debug, Clone)]
pub struct Measurements {
    // --- Statistical (always available) ---
    pub vmax: f64,
    pub vmin: f64,
    pub vpp: f64,
    pub vmean: f64,
    pub vrms: f64,

    // --- Time-domain (None when insufficient data) ---
    pub frequency: Option<f64>,
    pub period: Option<f64>,
    pub rise_time: Option<f64>,
    pub fall_time: Option<f64>,
    pub duty_cycle: Option<f64>,
    pub pos_width: Option<f64>,
    pub neg_width: Option<f64>,
}

impl Default for Measurements {
    fn default() -> Self {
        Self {
            vmax: f64::NAN,
            vmin: f64::NAN,
            vpp: f64::NAN,
            vmean: f64::NAN,
            vrms: f64::NAN,
            frequency: None,
            period: None,
            rise_time: None,
            fall_time: None,
            duty_cycle: None,
            pos_width: None,
            neg_width: None,
        }
    }
}

/// Maximum raw points for time-domain measurement calculations.
const MEASUREMENT_MAX_POINTS: usize = 200_000;

impl Measurements {
    /// Compute all measurements for a channel in the visible x-range.
    pub fn compute(data: &WaveformData, ch_idx: usize, vis_x_min: f64, vis_x_max: f64) -> Self {
        let mut m = Self::default();

        // --- Statistical measurements via Polars ---
        if let Ok(stats) = compute_stats(data, ch_idx, vis_x_min, vis_x_max) {
            m.vmax = stats.vmax;
            m.vmin = stats.vmin;
            m.vpp = stats.vpp;
            m.vmean = stats.vmean;
            m.vrms = stats.vrms;
        }

        // --- Time-domain measurements from raw points ---
        let points = data.get_raw_points(ch_idx, vis_x_min, vis_x_max, MEASUREMENT_MAX_POINTS);
        if points.len() >= 4 {
            let td = compute_time_domain(&points);
            m.frequency = td.frequency;
            m.period = td.period;
            m.rise_time = td.rise_time;
            m.fall_time = td.fall_time;
            m.duty_cycle = td.duty_cycle;
            m.pos_width = td.pos_width;
            m.neg_width = td.neg_width;
        }

        m
    }

    /// Format a measurement value with SI-prefixed unit.
    pub fn format_value(value: f64, unit: &str) -> String {
        if value.is_nan() || value.is_infinite() {
            return "---".to_owned();
        }
        let abs = value.abs();
        if abs == 0.0 {
            return format!("0 {}", unit);
        }
        const PREFIXES: &[(f64, &str)] = &[
            (1e12, "T"),
            (1e9, "G"),
            (1e6, "M"),
            (1e3, "k"),
            (1.0, ""),
            (1e-3, "m"),
            (1e-6, "u"),
            (1e-9, "n"),
            (1e-12, "p"),
        ];
        for &(scale, prefix) in PREFIXES {
            if abs >= scale * 0.999 {
                let v = value / scale;
                return format!("{:.4} {}{}", v, prefix, unit);
            }
        }
        format!("{:.4e} {}", value, unit)
    }
}

// ---------- statistical (Polars) ----------

struct Stats {
    vmax: f64,
    vmin: f64,
    vpp: f64,
    vmean: f64,
    vrms: f64,
}

fn compute_stats(
    data: &WaveformData,
    ch_idx: usize,
    vis_x_min: f64,
    vis_x_max: f64,
) -> Result<Stats, String> {
    use polars::prelude::*;

    let data_col = data
        .data_cols()
        .get(ch_idx)
        .ok_or_else(|| "Channel index out of range".to_owned())?
        .clone();
    let time_col = data.time_col().to_owned();

    let result = data
        .df()
        .clone()
        .lazy()
        .filter(
            col(&time_col)
                .gt_eq(lit(vis_x_min))
                .and(col(&time_col).lt_eq(lit(vis_x_max))),
        )
        .select([
            col(&data_col).max().alias("vmax"),
            col(&data_col).min().alias("vmin"),
            col(&data_col).mean().alias("vmean"),
            ((col(&data_col) * col(&data_col)).mean()).alias("mean_sq"),
        ])
        .collect()
        .map_err(|e| format!("Stats error: {e}"))?;

    let vmax = extract_f64(&result, "vmax");
    let vmin = extract_f64(&result, "vmin");
    let vmean = extract_f64(&result, "vmean");
    let mean_sq = extract_f64(&result, "mean_sq");

    Ok(Stats {
        vmax,
        vmin,
        vpp: vmax - vmin,
        vmean,
        vrms: mean_sq.sqrt(),
    })
}

fn extract_f64(df: &polars::prelude::DataFrame, name: &str) -> f64 {
    df.column(name)
        .ok()
        .and_then(|c| c.f64().ok())
        .and_then(|ca| ca.get(0))
        .unwrap_or(f64::NAN)
}

// ---------- time-domain (raw points) ----------

struct TimeDomainResult {
    frequency: Option<f64>,
    period: Option<f64>,
    rise_time: Option<f64>,
    fall_time: Option<f64>,
    duty_cycle: Option<f64>,
    pos_width: Option<f64>,
    neg_width: Option<f64>,
}

impl Default for TimeDomainResult {
    fn default() -> Self {
        Self {
            frequency: None,
            period: None,
            rise_time: None,
            fall_time: None,
            duty_cycle: None,
            pos_width: None,
            neg_width: None,
        }
    }
}

fn compute_time_domain(points: &[[f64; 2]]) -> TimeDomainResult {
    let mut result = TimeDomainResult::default();
    let n = points.len();
    if n < 4 {
        return result;
    }

    let y_vals: Vec<f64> = points.iter().map(|p| p[1]).collect();
    let vmax = y_vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let vmin = y_vals.iter().cloned().fold(f64::INFINITY, f64::min);
    let vpp = vmax - vmin;
    if vpp < f64::EPSILON {
        return result;
    }

    let mid = (vmax + vmin) / 2.0;
    let thresh_10 = vmin + 0.1 * vpp;
    let thresh_90 = vmin + 0.9 * vpp;

    // --- Frequency / Period via mid-level crossings ---
    let mut rising_edges: Vec<usize> = Vec::new();
    let mut falling_edges: Vec<usize> = Vec::new();
    for i in 1..n {
        if y_vals[i - 1] <= mid && y_vals[i] > mid {
            rising_edges.push(i);
        } else if y_vals[i - 1] > mid && y_vals[i] <= mid {
            falling_edges.push(i);
        }
    }

    // Period from consecutive rising edges
    if rising_edges.len() >= 2 {
        let periods: Vec<f64> = rising_edges
            .windows(2)
            .map(|w| points[w[1]][0] - points[w[0]][0])
            .collect();
        let avg_period = periods.iter().sum::<f64>() / periods.len() as f64;
        if avg_period > 0.0 {
            result.period = Some(avg_period);
            result.frequency = Some(1.0 / avg_period);
        }
    }

    // Duty cycle: fraction of time above mid-level
    if result.period.is_some() {
        let total_time = points[n - 1][0] - points[0][0];
        if total_time > 0.0 {
            let mut pos_time = 0.0;
            for i in 1..n {
                let dt = points[i][0] - points[i - 1][0];
                if y_vals[i] > mid {
                    pos_time += dt;
                }
            }
            let duty_pct = (pos_time / total_time) * 100.0;
            let period = result.period.unwrap();
            result.duty_cycle = Some(duty_pct);
            result.pos_width = Some(duty_pct / 100.0 * period);
            result.neg_width = Some((100.0 - duty_pct) / 100.0 * period);
        }
    }

    // --- Rise time (10% -> 90% on first rising edge) ---
    result.rise_time = find_edge_time(&points, &y_vals, thresh_10, thresh_90, true);

    // --- Fall time (90% -> 10% on first falling edge) ---
    result.fall_time = find_edge_time(&points, &y_vals, thresh_90, thresh_10, false);

    result
}

/// Find the time for a signal to transition between two thresholds on the first
/// matching edge. For a rising edge: `from` < `to`. For a falling edge: `from` > `to`.
fn find_edge_time(
    points: &[[f64; 2]],
    y_vals: &[f64],
    thresh_from: f64,
    thresh_to: f64,
    rising: bool,
) -> Option<f64> {
    let n = y_vals.len();
    for i in 1..n {
        let crossed_from = if rising {
            y_vals[i - 1] < thresh_from && y_vals[i] >= thresh_from
        } else {
            y_vals[i - 1] > thresh_from && y_vals[i] <= thresh_from
        };
        if !crossed_from {
            continue;
        }
        let t_from = interpolate_time(&points[i - 1], &points[i], thresh_from);

        // Now find where it reaches thresh_to
        for j in (i + 1)..n {
            let reached_to = if rising {
                y_vals[j - 1] < thresh_to && y_vals[j] >= thresh_to
            } else {
                y_vals[j - 1] > thresh_to && y_vals[j] <= thresh_to
            };
            if reached_to {
                let t_to = interpolate_time(&points[j - 1], &points[j], thresh_to);
                let dt = t_to - t_from;
                if dt > 0.0 {
                    return Some(dt);
                }
                break;
            }
        }
        return None; // Only measure the first edge
    }
    None
}

/// Linear interpolation: find x where y = target between two points.
fn interpolate_time(p0: &[f64; 2], p1: &[f64; 2], target: f64) -> f64 {
    let dy = p1[1] - p0[1];
    if dy.abs() < f64::EPSILON {
        return p0[0];
    }
    let t = (target - p0[1]) / dy;
    p0[0] + t * (p1[0] - p0[0])
}
