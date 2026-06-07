//! Persistence eye diagram (余晖眼图).
//!
//! Overlays UI-aligned waveform segments into a 2D density histogram,
//! then renders it as a colour-mapped heatmap.
//!
//! Design inspired by ngscopeclient (https://github.com/ngscopeclient):
//! - Sub-pixel anti-aliasing via `EYE_ACCUM_SCALE` (Y-direction weight splitting)
//! - Left-right symmetry: accumulate right half, mirror to left during normalisation
//! - Configurable saturation level
//! - External clock channel input
//! - Multiple colormaps including Viridis, Ironbow, CRT
//! - BER estimation from eye opening center

use egui::{Color32, Vec2};
use egui_plot::Plot;

use super::OscilloscopeApp;

// =======================================================================
// Constants
// =======================================================================

/// Sub-pixel anti-aliasing scale.
/// Each sample distributes its weight across two adjacent Y-bins:
/// `bin_main += ACCUM_SCALE - bin_frac` and `bin_next += bin_frac`.
/// This produces smoother eye patterns, matching ngscopeclient's approach.
const EYE_ACCUM_SCALE: u32 = 64;

// =======================================================================
// Colour maps
// =======================================================================
//
// Several colormaps use anchor-based piecewise-linear interpolation.
// The first anchor is typically at t≈0.01 so that even a single trace
// is visible against the black background.

/// Rainbow colormap (12-anchor, la-eyes / ngscopeclient style).
fn rainbow_color(t: f32) -> Color32 {
    const ANCHORS: &[(f32, f32, f32, f32)] = &[
        (0.00, 0.00, 0.00, 0.00), // black
        (0.01, 0.00, 0.05, 0.30), // dark blue — single traces visible
        (0.04, 0.00, 0.20, 0.70), // medium blue
        (0.10, 0.00, 0.45, 0.95), // bright blue
        (0.18, 0.00, 0.75, 0.95), // sky blue
        (0.28, 0.00, 0.92, 0.70), // teal
        (0.40, 0.15, 0.92, 0.15), // green
        (0.52, 0.60, 0.95, 0.00), // yellow-green
        (0.65, 1.00, 0.85, 0.00), // yellow
        (0.78, 1.00, 0.50, 0.00), // orange
        (0.90, 1.00, 0.15, 0.00), // red
        (1.00, 1.00, 1.00, 1.00), // white — peak density
    ];
    lerp_anchors(t, ANCHORS)
}

/// Monochrome density colormap — mimics overlaid traces in `base` colour.
fn mono_color(t: f32, base: Color32) -> Color32 {
    let br = base.r() as f32 / 255.0;
    let bg = base.g() as f32 / 255.0;
    let bb = base.b() as f32 / 255.0;
    let t = t.clamp(0.0, 1.0);

    const ANCHORS: &[(f32, f32)] = &[
        (0.000, 0.00),
        (0.005, 0.40),
        (0.020, 0.60),
        (0.080, 0.78),
        (0.200, 0.90),
        (0.400, 0.96),
        (0.600, 1.00),
    ];

    if t <= 0.6 {
        let fac = lerp_factor(t, ANCHORS);
        Color32::from_rgb(
            (br * fac * 255.0) as u8,
            (bg * fac * 255.0) as u8,
            (bb * fac * 255.0) as u8,
        )
    } else {
        let s = (t - 0.6) / 0.4;
        Color32::from_rgb(
            ((br + s * (1.0 - br)) * 255.0) as u8,
            ((bg + s * (1.0 - bg)) * 255.0) as u8,
            ((bb + s * (1.0 - bb)) * 255.0) as u8,
        )
    }
}

/// Temperature colormap: black → blue → cyan → green → yellow → red → white.
fn temperature_color(t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let (r, g, b);
    if t < 0.14 {
        let s = t / 0.14;
        r = 0;
        g = 0;
        b = (s * 180.0) as u8;
    } else if t < 0.28 {
        let s = (t - 0.14) / 0.14;
        r = 0;
        g = (s * 255.0) as u8;
        b = 180;
    } else if t < 0.42 {
        let s = (t - 0.28) / 0.14;
        r = 0;
        g = 255;
        b = (180.0 * (1.0 - s)) as u8;
    } else if t < 0.57 {
        let s = (t - 0.42) / 0.15;
        r = (s * 255.0) as u8;
        g = 255;
        b = 0;
    } else if t < 0.71 {
        let s = (t - 0.57) / 0.14;
        r = 255;
        g = (255.0 * (1.0 - s)) as u8;
        b = 0;
    } else if t < 0.85 {
        let s = (t - 0.71) / 0.14;
        r = 255;
        g = 0;
        b = (s * 200.0) as u8;
    } else {
        let s = (t - 0.85) / 0.15;
        r = 255;
        g = (s * 255.0) as u8;
        b = (200.0 + s * 55.0) as u8;
    }
    Color32::from_rgb(r, g, b)
}

/// Grayscale colormap.
fn grayscale_color(t: f32) -> Color32 {
    let v = (t.clamp(0.0, 1.0) * 255.0) as u8;
    Color32::from_rgb(v, v, v)
}

/// Green phosphor colormap (classic oscilloscope CRT look).
fn phosphor_color(t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let r = (t * 80.0) as u8;
    let g = (t * 255.0) as u8;
    let b = (t * 60.0) as u8;
    Color32::from_rgb(r, g, b)
}

/// Viridis colormap — perceptually uniform, friendly to colour-blind users.
/// Based on the reference 256-entry lookup table.
fn viridis_color(t: f32) -> Color32 {
    const ANCHORS: &[(f32, f32, f32, f32)] = &[
        (0.000, 0.267, 0.004, 0.329),
        (0.010, 0.283, 0.141, 0.458),
        (0.040, 0.254, 0.265, 0.530),
        (0.100, 0.207, 0.369, 0.553),
        (0.180, 0.164, 0.471, 0.558),
        (0.280, 0.128, 0.567, 0.551),
        (0.380, 0.135, 0.659, 0.518),
        (0.480, 0.217, 0.741, 0.462),
        (0.580, 0.345, 0.806, 0.388),
        (0.680, 0.504, 0.853, 0.302),
        (0.780, 0.677, 0.889, 0.213),
        (0.880, 0.853, 0.910, 0.118),
        (1.000, 0.993, 0.906, 0.144),
    ];
    lerp_anchors(t, ANCHORS)
}

/// Ironbow colormap — widely used in thermal imaging and astronomy.
/// Black → dark blue → purple → red → orange → yellow → white.
fn ironbow_color(t: f32) -> Color32 {
    const ANCHORS: &[(f32, f32, f32, f32)] = &[
        (0.00, 0.00, 0.00, 0.00),  // black
        (0.02, 0.04, 0.02, 0.15),  // very dark blue
        (0.08, 0.10, 0.05, 0.42),  // dark blue
        (0.18, 0.25, 0.08, 0.55),  // blue-purple
        (0.30, 0.50, 0.05, 0.45),  // purple
        (0.42, 0.75, 0.05, 0.20),  // red-purple
        (0.55, 0.90, 0.15, 0.05),  // red
        (0.68, 0.98, 0.40, 0.03),  // orange
        (0.80, 1.00, 0.70, 0.10),  // yellow-orange
        (0.92, 1.00, 0.95, 0.55),  // pale yellow
        (1.00, 1.00, 1.00, 1.00),  // white
    ];
    lerp_anchors(t, ANCHORS)
}

/// CRT amber phosphor colormap — classic amber oscilloscope display.
fn crt_amber_color(t: f32) -> Color32 {
    const ANCHORS: &[(f32, f32, f32, f32)] = &[
        (0.00, 0.00, 0.00, 0.00),
        (0.01, 0.20, 0.08, 0.00),
        (0.05, 0.40, 0.15, 0.00),
        (0.15, 0.60, 0.22, 0.00),
        (0.30, 0.75, 0.28, 0.00),
        (0.50, 0.88, 0.38, 0.02),
        (0.70, 0.96, 0.55, 0.05),
        (0.85, 1.00, 0.75, 0.20),
        (1.00, 1.00, 1.00, 0.70),
    ];
    lerp_anchors(t, ANCHORS)
}

// -- helpers for anchor-based interpolation --

fn lerp_anchors(t: f32, anchors: &[(f32, f32, f32, f32)]) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let mut lo = 0;
    for (i, &(pos, _, _, _)) in anchors.iter().enumerate() {
        if pos <= t {
            lo = i;
        } else {
            break;
        }
    }
    let (lp, lr, lg, lb) = anchors[lo];
    let (hp, hr, hg, hb) = anchors[(lo + 1).min(anchors.len() - 1)];
    let s = if hp > lp {
        (t - lp) / (hp - lp)
    } else {
        0.0
    };
    Color32::from_rgb(
        ((lr + s * (hr - lr)) * 255.0) as u8,
        ((lg + s * (hg - lg)) * 255.0) as u8,
        ((lb + s * (hb - lb)) * 255.0) as u8,
    )
}

fn lerp_factor(t: f32, anchors: &[(f32, f32)]) -> f32 {
    let mut lo = 0;
    for (i, &(pos, _)) in anchors.iter().enumerate() {
        if pos <= t {
            lo = i;
        } else {
            break;
        }
    }
    let (lp, lf) = anchors[lo];
    let (hp, hf) = anchors[(lo + 1).min(anchors.len() - 1)];
    if hp > lp {
        lf + (t - lp) / (hp - lp) * (hf - lf)
    } else {
        lf
    }
}

// =======================================================================
// Eye diagram state
// =======================================================================

/// Persistent state for the eye diagram computation.
///
/// The grid stores `u64` accumulator values (not `u32`) to match
/// ngscopeclient's `int64_t` approach — this avoids precision loss when
/// many samples are integrated.  Sub-pixel anti-aliasing is done by
/// distributing `EYE_ACCUM_SCALE` counts across two adjacent Y-bins per
/// sample.
pub(crate) struct EyeDiagramState {
    /// 2D accumulation grid (row-major): grid[y * grid_x + x].
    /// Stored as flat `Vec<u64>` for cache-friendly access.
    pub accum: Vec<u64>,
    /// Width of the grid (time axis bins).
    pub grid_x: usize,
    /// Height of the grid (voltage axis bins).
    pub grid_y: usize,
    /// Y value range: (y_min, y_max).
    pub y_range: (f64, f64),
    /// Total UI segments overlaid.
    pub n_segments: usize,
    /// Total raw samples accumulated (for mask hit-rate).
    pub n_samples: usize,
    /// Cached texture for rendering.
    pub texture: Option<egui::TextureHandle>,
    /// Whether the grid needs re-rendering to texture.
    pub dirty: bool,
    /// Maximum accumulator value (for normalisation).
    pub max_count: u64,
    /// Whether the plot view should be reset to fit data.
    pub reset_view: bool,
}

impl EyeDiagramState {
    pub fn new(grid_x: usize, grid_y: usize) -> Self {
        Self {
            accum: vec![0u64; grid_x * grid_y],
            grid_x,
            grid_y,
            y_range: (0.0, 1.0),
            n_segments: 0,
            n_samples: 0,
            texture: None,
            dirty: true,
            max_count: 0,
            reset_view: true,
        }
    }

    /// Clear the accumulation grid.
    pub fn clear(&mut self) {
        self.accum.fill(0);
        self.n_segments = 0;
        self.n_samples = 0;
        self.max_count = 0;
        self.dirty = true;
        self.reset_view = true;
    }

    /// Accumulate a single segment of points into the grid with
    /// **sub-pixel anti-aliasing** (ngscopeclient-inspired).
    ///
    /// Each sample distributes `EYE_ACCUM_SCALE` counts between two
    /// adjacent Y-bins based on the fractional position, producing a
    /// smoother eye pattern than simple nearest-bin accumulation.
    ///
    /// `segment`: list of (t_offset, voltage) where t_offset ∈ [0, total_width).
    /// `total_width`: full segment width in seconds.
    fn accumulate_segment(&mut self, segment: &[(f64, f64)], total_width: f64) {
        let gx = self.grid_x;
        let gy = self.grid_y;
        let y_lo = self.y_range.0;
        let y_range = self.y_range.1 - y_lo;

        for &(t_off, y) in segment {
            if t_off < 0.0 || t_off >= total_width || y_range <= 0.0 {
                continue;
            }

            // X bin
            let x_bin = ((t_off / total_width) * gx as f64) as usize;
            let x_bin = x_bin.min(gx - 1);

            // Y bin (flip: top = high voltage, like ngscopeclient)
            let y_norm = (y - y_lo) / y_range;           // 0..1
            let y_pixel = (1.0 - y_norm) * (gy as f64);  // top=high voltage
            let y_floor = y_pixel.floor();
            let y_frac = y_pixel - y_floor;              // fractional part

            let y1 = y_floor as isize;
            if y1 < 0 || y1 >= gy as isize {
                continue;
            }
            let y1 = y1 as usize;

            // Sub-pixel weight distribution (ngscopeclient-style)
            let bin_frac = (y_frac * EYE_ACCUM_SCALE as f64) as u32;
            let bin_main = EYE_ACCUM_SCALE - bin_frac;

            let idx = y1 * gx + x_bin;
            self.accum[idx] += bin_main as u64;

            // Distribute remainder to next row (if in bounds)
            if y1 + 1 < gy {
                self.accum[idx + gx] += bin_frac as u64;
            }
        }
        self.n_segments += 1;
        self.n_samples += segment.len();
    }

    /// Normalise the full accumulation grid.
    ///
    /// For 2-UI mode, after normalisation the right half is copied to the
    /// left half to produce a perfectly symmetric eye (ngscopeclient style).
    /// For other UI counts, the full grid is normalised as-is.
    ///
    /// This method does **not** mutate `self.accum` — it reads the
    /// accumulator and returns a fresh normalised `Vec<f32>` so that
    /// switching colour modes never destroys accumulated data.
    fn normalize(&mut self, saturation: f32, n_ui: usize) -> Vec<f32> {
        let gx = self.grid_x;
        let gy = self.grid_y;

        // Find global peak over the entire grid
        let nmax = self.accum.iter().copied().max().unwrap_or(0);
        self.max_count = nmax;

        let divisor = if nmax == 0 { 1.0f32 } else { nmax as f32 };
        let norm = saturation / divisor;

        let mut out: Vec<f32> = self
            .accum
            .iter()
            .map(|&v| (v as f32 * norm).min(1.0))
            .collect();

        // Symmetry: for 2-UI mode, mirror right half → left half
        if n_ui == 2 {
            let half = gx / 2;
            for y in 0..gy {
                let row = y * gx;
                for x in 0..half {
                    out[row + x] = out[row + x + half];
                }
            }
        }

        out
    }

    /// Convert the accumulation grid to a ColorImage using the selected
    /// colormap and saturation level.
    fn to_color_image(
        &mut self,
        color_mode: EyeColorMode,
        base_color: Color32,
        saturation: f32,
        n_ui: usize,
    ) -> egui::ColorImage {
        let gx = self.grid_x;
        let gy = self.grid_y;

        let normed = self.normalize(saturation, n_ui);

        let pixels: Vec<Color32> = normed
            .chunks_exact(gx)
            .flat_map(|row| {
                row.iter().map(|&t| match color_mode {
                    EyeColorMode::Rainbow => rainbow_color(t),
                    EyeColorMode::Monochrome => mono_color(t, base_color),
                    EyeColorMode::Temperature => temperature_color(t),
                    EyeColorMode::Grayscale => grayscale_color(t),
                    EyeColorMode::Phosphor => phosphor_color(t),
                    EyeColorMode::Viridis => viridis_color(t),
                    EyeColorMode::Ironbow => ironbow_color(t),
                    EyeColorMode::CrtAmber => crt_amber_color(t),
                })
            })
            .collect();

        egui::ColorImage {
            size: [gx, gy],
            pixels,
        }
    }
}

// =======================================================================
// Colour mode enum
// =======================================================================

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum EyeColorMode {
    Rainbow,
    Monochrome,
    Temperature,
    Grayscale,
    Phosphor,
    Viridis,
    Ironbow,
    CrtAmber,
}

impl EyeColorMode {
    /// All available colour modes in display order.
    const ALL: [EyeColorMode; 8] = [
        EyeColorMode::Monochrome,
        EyeColorMode::Rainbow,
        EyeColorMode::Temperature,
        EyeColorMode::Grayscale,
        EyeColorMode::Phosphor,
        EyeColorMode::Viridis,
        EyeColorMode::Ironbow,
        EyeColorMode::CrtAmber,
    ];
}

impl std::fmt::Display for EyeColorMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EyeColorMode::Rainbow => write!(f, "Rainbow"),
            EyeColorMode::Monochrome => write!(f, "Monochrome"),
            EyeColorMode::Temperature => write!(f, "Temperature"),
            EyeColorMode::Grayscale => write!(f, "Grayscale"),
            EyeColorMode::Phosphor => write!(f, "Phosphor"),
            EyeColorMode::Viridis => write!(f, "Viridis"),
            EyeColorMode::Ironbow => write!(f, "Ironbow"),
            EyeColorMode::CrtAmber => write!(f, "CRT Amber"),
        }
    }
}

// =======================================================================
// Clock polarity
// =======================================================================

/// Which clock edges to use as trigger points.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClockPolarity {
    Rising,
    Falling,
    Both,
}

impl ClockPolarity {
    const ALL: [ClockPolarity; 3] = [
        ClockPolarity::Rising,
        ClockPolarity::Falling,
        ClockPolarity::Both,
    ];
}

impl std::fmt::Display for ClockPolarity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClockPolarity::Rising => write!(f, "Rising"),
            ClockPolarity::Falling => write!(f, "Falling"),
            ClockPolarity::Both => write!(f, "Both"),
        }
    }
}

// =======================================================================
// UI
// =======================================================================

impl OscilloscopeApp {
    pub(crate) fn draw_eye_diagram(&mut self, ctx: &egui::Context) {
        if self.data.is_none() {
            return;
        }

        // Ensure the eye diagram state exists.
        if self.eye_state.is_none() {
            self.eye_state = Some(EyeDiagramState::new(
                self.eye_grid_x,
                self.eye_grid_y,
            ));
        }

        let eye_ui_period = self.eye_ui_period;
        let eye_channel = self.eye_channel;
        let n_ch = self.data.as_ref().map(|d| d.n_channels()).unwrap_or(0);
        let eye_auto_threshold = self.eye_auto_threshold;
        let eye_n_ui = self.eye_n_ui;
        let total_width = eye_n_ui as f64 * eye_ui_period;
        let prev_color_mode = self.eye_color_mode;

        // Deferred action flags — processed after the window closure.
        let mut request_auto_detect = false;
        let mut request_recompute = false;
        let mut request_clear = false;

        egui::Window::new("Eye Diagram")
            .open(&mut self.show_eye_window)
            .default_size([780.0, 560.0])
            .min_size([500.0, 350.0])
            .show(ctx, |ui| {
                // ── Controls row 1 ──
                ui.horizontal(|ui| {
                    ui.label("Signal:");
                    egui::ComboBox::from_id_salt("eye_ch_select")
                        .selected_text(
                            self.channels
                                .get(eye_channel)
                                .map(|c| c.name.as_str())
                                .unwrap_or("---"),
                        )
                        .show_ui(ui, |ui| {
                            for i in 0..n_ch.min(self.channels.len()) {
                                ui.selectable_value(
                                    &mut self.eye_channel,
                                    i,
                                    &self.channels[i].name,
                                );
                            }
                        });

                    ui.separator();

                    // Clock channel selector
                    ui.label("Clock:");
                    let clock_label = self
                        .eye_clock_channel
                        .map(|c| self.channels.get(c).map(|ch| ch.name.as_str()).unwrap_or("---"))
                        .unwrap_or("Auto (self)");
                    egui::ComboBox::from_id_salt("eye_clk_select")
                        .selected_text(clock_label)
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut self.eye_clock_channel, None, "Auto (self)");
                            for i in 0..n_ch.min(self.channels.len()) {
                                ui.selectable_value(
                                    &mut self.eye_clock_channel,
                                    Some(i),
                                    &self.channels[i].name,
                                );
                            }
                        });

                    ui.separator();

                    // Clock polarity
                    ui.label("Edge:");
                    egui::ComboBox::from_id_salt("eye_clk_pol")
                        .selected_text(self.eye_clock_polarity.to_string())
                        .show_ui(ui, |ui| {
                            for pol in ClockPolarity::ALL {
                                ui.selectable_value(
                                    &mut self.eye_clock_polarity,
                                    pol,
                                    pol.to_string(),
                                );
                            }
                        });

                    ui.separator();

                    let ns_str = format!("({:.4} ns)", self.eye_ui_period * 1e9);
                    ui.label(format!("UI {}:", ns_str));
                    let changed = ui.add(
                        egui::TextEdit::singleline(&mut self.eye_ui_period_str)
                            .desired_width(80.0)
                            .hint_text("e.g. 1e-9"),
                    ).lost_focus();
                    if changed || ui.button("Set").clicked() {
                        if let Ok(v) = self.eye_ui_period_str.parse::<f64>() {
                            if v > 0.0 {
                                self.eye_ui_period = v;
                                self.eye_needs_recompute = true;
                            }
                        }
                    }
                });

                // ── Controls row 2 ──
                ui.horizontal(|ui| {
                    ui.label("Color:");
                    egui::ComboBox::from_id_salt("eye_color_select")
                        .selected_text(self.eye_color_mode.to_string())
                        .show_ui(ui, |ui| {
                            for mode in EyeColorMode::ALL {
                                ui.selectable_value(&mut self.eye_color_mode, mode, mode.to_string());
                            }
                        });

                    ui.separator();

                    ui.label("UIs:");
                    let mut n_tmp = self.eye_n_ui as u32;
                    if ui.add(
                        egui::DragValue::new(&mut n_tmp).range(2..=8).speed(0.1),
                    ).changed() {
                        self.eye_n_ui = n_tmp as usize;
                        self.eye_needs_recompute = true;
                    }

                    ui.separator();

                    // Saturation level
                    ui.label("Saturation:");
                    let mut sat_tmp = self.eye_saturation;
                    if ui.add(
                        egui::DragValue::new(&mut sat_tmp)
                            .range(0.5..=4.0)
                            .speed(0.05)
                            .custom_formatter(|v, _| format!("{:.1}", v)),
                    ).changed() {
                        self.eye_saturation = sat_tmp;
                        if let Some(ref mut state) = self.eye_state {
                            state.dirty = true;
                        }
                    }

                    ui.separator();

                    if ui.button("Recompute").clicked() {
                        request_recompute = true;
                    }
                    if ui.button("Clear").clicked() {
                        request_clear = true;
                    }
                    if ui.button("Auto UI").on_hover_text(
                        "Detect UI period from signal edges\n(uses Schmitt-trigger + gap clustering)",
                    ).clicked() {
                        request_auto_detect = true;
                    }
                });

                // ── Detect parameter changes ──
                if self.eye_channel != eye_channel {
                    self.eye_needs_recompute = true;
                }
                if self.eye_color_mode != prev_color_mode {
                    if let Some(ref mut state) = self.eye_state {
                        state.dirty = true;
                    }
                }

                // ── Info bar ──
                let n_segments = self.eye_state.as_ref().map(|s| s.n_segments).unwrap_or(0);
                let n_samples = self.eye_state.as_ref().map(|s| s.n_samples).unwrap_or(0);
                ui.horizontal(|ui| {
                    ui.label(format!("Segments: {}  |  Samples: {}", n_segments, n_samples));
                    ui.separator();
                    ui.label(format!(
                        "{} UI × {:.4e} s = {:.4e} s",
                        eye_n_ui, eye_ui_period, total_width,
                    ));
                    if eye_auto_threshold {
                        ui.label("(auto)");
                    }
                    ui.separator();
                    ui.label(format!(
                        "Grid: {}×{}  |  Sat: {:.1}",
                        self.eye_grid_x, self.eye_grid_y, self.eye_saturation,
                    ));
                });

                ui.add_space(4.0);

                // ── Render ──
                let state = self.eye_state.as_mut().unwrap();
                let avail = ui.available_size();
                let plot_h = avail.y - 10.0;

                if avail.x > 10.0 && plot_h > 10.0 {
                    let y_min = state.y_range.0;
                    let y_max = state.y_range.1;
                    let ch_color = self.channels.get(eye_channel)
                        .map(|c| c.color)
                        .unwrap_or(Color32::WHITE);

                    // Update heatmap texture if needed
                    if state.dirty || state.texture.is_none() {
                        let img = state.to_color_image(
                            self.eye_color_mode,
                            ch_color,
                            self.eye_saturation,
                            self.eye_n_ui,
                        );
                        let texture = ui.ctx().load_texture(
                            "eye_heatmap",
                            img,
                            egui::TextureOptions::LINEAR,
                        );
                        state.texture = Some(texture);
                        state.dirty = false;
                    }

                    let n_ui_f = eye_n_ui as f64;

                    Plot::new("eye_plot")
                        .show_grid([true, true])
                        .x_axis_label("Time (UI)")
                        .y_axis_label("Voltage")
                        .allow_zoom([true, true])
                        .allow_drag([true, true])
                        .allow_double_click_reset(true)
                        .set_margin_fraction(Vec2::new(0.05, 0.05))
                        .height(plot_h)
                        .show(ui, |plot_ui| {
                            if state.reset_view {
                                plot_ui.set_plot_bounds(egui_plot::PlotBounds::from_min_max(
                                    [0.0, y_min],
                                    [n_ui_f, y_max],
                                ));
                                state.reset_view = false;
                            }

                            if let Some(ref texture) = state.texture {
                                use egui_plot::PlotImage;
                                let img = PlotImage::new(
                                    texture,
                                    egui_plot::PlotPoint::new(
                                        n_ui_f / 2.0,
                                        (y_min + y_max) / 2.0,
                                    ),
                                    Vec2::new(
                                        n_ui_f as f32,
                                        (y_max - y_min) as f32,
                                    ),
                                );
                                plot_ui.image(img);
                            }
                        });
                }
            });

        // ── Deferred actions ──
        if request_clear {
            if let Some(ref mut state) = self.eye_state {
                state.clear();
            }
        }
        if request_auto_detect {
            self.do_auto_detect_ui_period();
        }
        if request_recompute || self.eye_needs_recompute {
            self.do_compute_eye_diagram();
            self.eye_needs_recompute = false;
        }
    }

    // ===================================================================
    // Auto-detect UI period
    // ===================================================================

    /// Auto-detect UI period using hysteresis edge detection + gap-based
    /// period clustering (inspired by ngscopeclient).
    ///
    /// Algorithm:
    /// 1. Schmitt-trigger edge detection (hysteresis) avoids noise doubling.
    /// 2. Inter-crossing periods are sorted; the first ratio gap > 1.5
    ///    separates the true UI cluster from 2×UI, 3×UI runs.
    /// 3. Discard top/bottom 10% and average the rest (ngscopeclient approach).
    fn do_auto_detect_ui_period(&mut self) {
        let vis_x_min = self.last_bounds.min()[0];
        let vis_x_max = self.last_bounds.max()[0];
        let ch = self.eye_channel;
        let points = self.data.as_ref()
            .map(|d| d.get_raw_points(ch, vis_x_min, vis_x_max, 200_000))
            .unwrap_or_default();

        if points.len() < 10 {
            self.status_message = "Not enough data to auto-detect UI period".to_owned();
            return;
        }

        // ── 1. Robust voltage statistics (quartile-based) ──
        let mut volts: Vec<f64> = points.iter().map(|p| p[1]).collect();
        volts.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let n = volts.len();
        let v_q25 = volts[n / 4];
        let v_q75 = volts[3 * n / 4];
        let v_swing = v_q75 - v_q25;
        let threshold = (v_q25 + v_q75) / 2.0;

        // ── 2. Schmitt-trigger edge detection ──
        let hyst = v_swing * 0.10;
        let thresh_hi = threshold + hyst;
        let thresh_lo = threshold - hyst;

        let mut crossings: Vec<f64> = Vec::new();
        let mut is_high = false;
        for i in 1..points.len() {
            let prev = points[i - 1][1];
            let curr = points[i][1];
            if !is_high && prev <= thresh_hi && curr > thresh_hi {
                let frac = (thresh_hi - prev) / (curr - prev);
                let t_cross = points[i - 1][0] + frac * (points[i][0] - points[i - 1][0]);
                crossings.push(t_cross);
                is_high = true;
            } else if is_high && curr < thresh_lo {
                is_high = false;
            }
        }

        if crossings.len() < 3 {
            self.status_message =
                "Cannot detect signal period: too few rising edges".to_owned();
            return;
        }

        // ── 3. Inter-crossing periods → sort ascending ──
        let mut periods: Vec<f64> = crossings
            .windows(2)
            .map(|w| w[1] - w[0])
            .filter(|&dt| dt > 0.0)
            .collect();
        periods.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

        if periods.is_empty() {
            self.status_message = "Cannot detect signal period".to_owned();
            return;
        }

        // ── 4. Gap-based UI period detection ──
        let ui_period = {
            let mut gap_end = periods.len();
            for i in 1..periods.len() {
                let ratio = periods[i] / periods[i - 1].max(1e-30);
                if ratio > 1.5 {
                    gap_end = i;
                    break;
                }
            }
            let cluster = &periods[..gap_end];

            // ngscopeclient approach: discard top/bottom 10%, average the rest
            let count = cluster.len();
            let lo = count / 10;
            let hi = count * 9 / 10;
            if hi > lo {
                let total: f64 = cluster[lo..=hi].iter().sum();
                total / (hi - lo + 1) as f64
            } else {
                cluster[cluster.len() / 2]
            }
        };

        self.eye_ui_period = ui_period;
        self.eye_ui_period_str = format!("{:.6e}", ui_period);
        self.eye_auto_threshold = true;
        self.eye_n_ui = 3;
        self.eye_needs_recompute = true;
        self.status_message = format!(
            "Auto-detected UI: {:.4e} s ({:.2} MHz), {} UIs displayed",
            ui_period,
            1.0 / ui_period / 1e6,
            self.eye_n_ui,
        );
    }

    // ===================================================================
    // Compute eye diagram
    // ===================================================================

    /// Compute the eye diagram by extracting segments and accumulating
    /// into the grid with sub-pixel anti-aliasing.
    ///
    /// Clock recovery uses either:
    /// - An external clock channel (if `eye_clock_channel` is set), or
    /// - Self-clocking via Schmitt-trigger edge detection on the signal itself.
    fn do_compute_eye_diagram(&mut self) {
        let vis_x_min = self.last_bounds.min()[0];
        let vis_x_max = self.last_bounds.max()[0];
        let ch_idx = self.eye_channel;
        let ui_period = self.eye_ui_period;
        let n_ui = self.eye_n_ui;

        if ui_period <= 0.0 || n_ui < 2 {
            return;
        }

        let total_width = n_ui as f64 * ui_period;

        // Get raw points for the signal channel
        let points = self.data.as_ref()
            .map(|d| d.get_raw_points(ch_idx, vis_x_min, vis_x_max, 500_000))
            .unwrap_or_default();

        if points.is_empty() {
            return;
        }

        // ── Y range: full signal min/max with 5% padding ──
        let mut volts: Vec<f64> = points.iter().map(|p| p[1]).collect();
        volts.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let n = volts.len();
        let y_lo = volts[0];
        let y_hi = volts[n - 1];
        let margin = (y_hi - y_lo) * 0.05;
        let y_range = (y_lo - margin, y_hi + margin);

        // Reset state
        if let Some(ref mut state) = self.eye_state {
            state.clear();
            state.y_range = y_range;
        }

        // ── Find clock edges ──
        let trigger_points = if let Some(clk_idx) = self.eye_clock_channel {
            // External clock channel
            self.find_clock_edges(clk_idx, vis_x_min, vis_x_max, &points)
        } else {
            // Self-clocking: Schmitt-trigger on the signal itself
            self.find_signal_edges(&points, &volts)
        };

        // Fallback: fixed-period segmentation if no edges found
        let trigger_points = if trigger_points.is_empty() {
            let start_time = points[0][0];
            let mut t = start_time;
            let mut idx = 0;
            let mut tp = Vec::new();
            while idx < points.len() {
                tp.push(idx);
                t += ui_period;
                while idx < points.len() && points[idx][0] < t {
                    idx += 1;
                }
            }
            tp
        } else {
            trigger_points
        };

        // ── Extract segments and accumulate ──
        let state = self.eye_state.as_mut().unwrap();

        for &start_idx in &trigger_points {
            let t_start = points[start_idx][0];
            let t_end = t_start + total_width;

            let mut segment: Vec<(f64, f64)> = Vec::new();
            let mut idx = start_idx;
            while idx < points.len() && points[idx][0] < t_end {
                let t_off = points[idx][0] - t_start;
                segment.push((t_off, points[idx][1]));
                idx += 1;
            }

            if !segment.is_empty() {
                state.accumulate_segment(&segment, total_width);
            }
        }

        state.dirty = true;
        state.reset_view = true;
    }

    // ===================================================================
    // Edge detection helpers
    // ===================================================================

    /// Find trigger points on the signal itself using Schmitt-trigger
    /// edge detection, respecting the `eye_clock_polarity` setting.
    fn find_signal_edges(
        &self,
        points: &[[f64; 2]],
        volts: &[f64],
    ) -> Vec<usize> {
        let n = volts.len();
        let v_q25 = volts[n / 4];
        let v_q75 = volts[3 * n / 4];
        let threshold = (v_q25 + v_q75) / 2.0;
        let hyst = (v_q75 - v_q25) * 0.10;
        let thresh_hi = threshold + hyst;
        let thresh_lo = threshold - hyst;

        let polarity = self.eye_clock_polarity;
        let mut trigger_points: Vec<usize> = Vec::new();
        let mut is_high = false;

        match polarity {
            ClockPolarity::Rising => {
                for i in 1..points.len() {
                    let prev = points[i - 1][1];
                    let curr = points[i][1];
                    if !is_high && prev <= thresh_hi && curr > thresh_hi {
                        trigger_points.push(i);
                        is_high = true;
                    } else if is_high && curr < thresh_lo {
                        is_high = false;
                    }
                }
            }
            ClockPolarity::Falling => {
                let mut is_low = false;
                for i in 1..points.len() {
                    let prev = points[i - 1][1];
                    let curr = points[i][1];
                    if !is_low && prev >= thresh_lo && curr < thresh_lo {
                        trigger_points.push(i);
                        is_low = true;
                    } else if is_low && curr > thresh_hi {
                        is_low = false;
                    }
                }
            }
            ClockPolarity::Both => {
                // Use both rising and falling edges
                let mut state: i8 = 0; // -1=low, 0=mid, 1=high
                for i in 1..points.len() {
                    let curr = points[i][1];
                    match state {
                        -1 | 0 => {
                            if curr > thresh_hi {
                                trigger_points.push(i);
                                state = 1;
                            }
                        }
                        1 | _ => {
                            if curr < thresh_lo {
                                trigger_points.push(i);
                                state = -1;
                            }
                        }
                    }
                }
            }
        }

        trigger_points
    }

    /// Find trigger points from an external clock channel.
    ///
    /// Detects edges on the clock channel and returns indices into `signal_points`
    /// that are closest to each clock edge time.
    fn find_clock_edges(
        &self,
        clk_idx: usize,
        vis_x_min: f64,
        vis_x_max: f64,
        signal_points: &[[f64; 2]],
    ) -> Vec<usize> {
        let clk_points = self.data.as_ref()
            .map(|d| d.get_raw_points(clk_idx, vis_x_min, vis_x_max, 200_000))
            .unwrap_or_default();

        if clk_points.len() < 2 || signal_points.is_empty() {
            return Vec::new();
        }

        // Determine clock threshold (midpoint of clock signal range)
        let mut clk_volts: Vec<f64> = clk_points.iter().map(|p| p[1]).collect();
        clk_volts.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let cn = clk_volts.len();
        let clk_mid = (clk_volts[cn / 4] + clk_volts[3 * cn / 4]) / 2.0;

        // Find clock edge times
        let mut edge_times: Vec<f64> = Vec::new();
        let polarity = self.eye_clock_polarity;

        match polarity {
            ClockPolarity::Rising => {
                for i in 1..clk_points.len() {
                    if clk_points[i - 1][1] <= clk_mid && clk_points[i][1] > clk_mid {
                        let frac = (clk_mid - clk_points[i - 1][1])
                            / (clk_points[i][1] - clk_points[i - 1][1]);
                        let t = clk_points[i - 1][0]
                            + frac * (clk_points[i][0] - clk_points[i - 1][0]);
                        edge_times.push(t);
                    }
                }
            }
            ClockPolarity::Falling => {
                for i in 1..clk_points.len() {
                    if clk_points[i - 1][1] >= clk_mid && clk_points[i][1] < clk_mid {
                        let frac = (clk_points[i - 1][1] - clk_mid)
                            / (clk_points[i - 1][1] - clk_points[i][1]);
                        let t = clk_points[i - 1][0]
                            + frac * (clk_points[i][0] - clk_points[i - 1][0]);
                        edge_times.push(t);
                    }
                }
            }
            ClockPolarity::Both => {
                for i in 1..clk_points.len() {
                    let prev = clk_points[i - 1][1];
                    let curr = clk_points[i][1];
                    if (prev <= clk_mid && curr > clk_mid)
                        || (prev >= clk_mid && curr < clk_mid)
                    {
                        let frac = if curr > prev {
                            (clk_mid - prev) / (curr - prev)
                        } else {
                            (prev - clk_mid) / (prev - curr)
                        };
                        let t = clk_points[i - 1][0]
                            + frac * (clk_points[i][0] - clk_points[i - 1][0]);
                        edge_times.push(t);
                    }
                }
            }
        }

        if edge_times.is_empty() {
            return Vec::new();
        }

        // Map clock edge times → nearest signal sample indices
        let mut result: Vec<usize> = Vec::new();
        let mut sig_idx: usize = 0;

        for &edge_t in &edge_times {
            // Advance sig_idx until we pass the edge time
            while sig_idx + 1 < signal_points.len()
                && signal_points[sig_idx][0] < edge_t
            {
                sig_idx += 1;
            }
            // Pick the closest sample
            if sig_idx > 0
                && (signal_points[sig_idx][0] - edge_t).abs()
                    > (signal_points[sig_idx - 1][0] - edge_t).abs()
            {
                result.push(sig_idx - 1);
            } else {
                result.push(sig_idx);
            }
        }

        result
    }
}
