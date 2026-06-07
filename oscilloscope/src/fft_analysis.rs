//! FFT spectrum analysis for oscilloscope channels.
//!
//! Computes the magnitude spectrum of a channel's visible-range data using
//! the `rustfft` crate. Supports rectangle, Hanning, and Blackman-Harris
//! window functions.

use std::f64::consts::PI;
use rustfft::{FftPlanner, num_complex::Complex64};

/// Window function applied before FFT.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WindowType {
    Rectangle,
    Hanning,
    BlackmanHarris,
}

impl std::fmt::Display for WindowType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rectangle => write!(f, "Rectangle"),
            Self::Hanning => write!(f, "Hanning"),
            Self::BlackmanHarris => write!(f, "Blackman-Harris"),
        }
    }
}

/// Display unit for the FFT magnitude.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FftScale {
    /// 20·log₁₀(magnitude) dB
    Db,
    /// Linear magnitude
    Linear,
}

/// Maximum raw samples fed into the FFT.
const FFT_MAX_SAMPLES: usize = 131_072; // power of 2 cap

/// Compute the FFT magnitude spectrum from `[time, value]` points.
///
/// Returns `Vec<[frequency_Hz, magnitude]>` for the positive-frequency half.
/// The input is detrended (mean-subtracted), windowed, zero-padded to a power
/// of 2, and transformed.  The output magnitude depends on `scale`.
pub fn compute_fft(
    points: &[[f64; 2]],
    window: WindowType,
    scale: FftScale,
) -> Vec<[f64; 2]> {
    let n = points.len();
    if n < 4 {
        return Vec::new();
    }

    // Cap input length
    let n = n.min(FFT_MAX_SAMPLES);
    let points = &points[..n];

    // Estimate sample rate from the time span
    let dt = if n > 1 {
        (points[n - 1][0] - points[0][0]) / (n - 1) as f64
    } else {
        1.0
    };
    if dt <= 0.0 {
        return Vec::new();
    }
    let sample_rate = 1.0 / dt;

    // Compute mean for detrending
    let mean: f64 = points.iter().map(|p| p[1]).sum::<f64>() / n as f64;

    // Build window
    let win: Vec<f64> = match window {
        WindowType::Rectangle => vec![1.0; n],
        WindowType::Hanning => (0..n)
            .map(|i| 0.5 * (1.0 - (2.0 * PI * i as f64 / (n - 1) as f64).cos()))
            .collect(),
        WindowType::BlackmanHarris => {
            let a0 = 0.35875;
            let a1 = 0.48829;
            let a2 = 0.14128;
            let a3 = 0.01168;
            (0..n)
                .map(|i| {
                    let nm1 = (n - 1) as f64;
                    let t = 2.0 * PI * i as f64 / nm1;
                    a0 - a1 * t.cos() + a2 * (2.0 * t).cos() - a3 * (3.0 * t).cos()
                })
                .collect()
        }
    };

    // Window coherence factor (for amplitude correction)
    let win_sum: f64 = win.iter().sum();

    // Prepare complex buffer, zero-pad to next power of 2
    let fft_len = n.next_power_of_two();
    let mut buffer: Vec<Complex64> = (0..fft_len)
        .map(|i| {
            if i < n {
                Complex64::new((points[i][1] - mean) * win[i], 0.0)
            } else {
                Complex64::new(0.0, 0.0)
            }
        })
        .collect();

    // Execute forward FFT
    let mut planner = FftPlanner::new();
    let fft = planner.plan_fft_forward(fft_len);
    fft.process(&mut buffer);

    // Build magnitude spectrum (positive frequencies only: 0 … N/2)
    let half = fft_len / 2;
    let norm = win_sum * 0.5; // amplitude normalisation
    let mut spectrum = Vec::with_capacity(half);
    for k in 0..=half {
        let freq = k as f64 * sample_rate / fft_len as f64;
        let mag = buffer[k].norm() / norm;
        let value = match scale {
            FftScale::Linear => mag,
            FftScale::Db => {
                if mag > 1e-15 {
                    20.0 * mag.log10()
                } else {
                    -200.0
                }
            }
        };
        spectrum.push([freq, value]);
    }

    spectrum
}
