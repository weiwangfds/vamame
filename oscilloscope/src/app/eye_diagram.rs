//! Eye diagram analysis window.
//!
//! Generates an eye diagram by overlaying waveform segments aligned to clock
//! edges. The result is rendered as a 2D density heatmap inside an egui_plot.
//!
//! Algorithm (inspired by eyediagram/stateye):
//! 1. Detect clock edges via Schmitt-trigger hysteresis.
//! 2. Extract N UI-wide segments from each trigger point.
//! 3. Rasterise each segment into a 2D grid with sub-pixel anti-aliasing.
//! 4. Map the grid counts to colour and display as a texture.

use egui::{Color32, Vec2};
use egui_plot::{Plot, PlotBounds, PlotImage, PlotPoint};

use super::OscilloscopeApp;

// =======================================================================
// Eye diagram state
// =======================================================================

/// Persistent state for the eye diagram window.
#[derive(Clone)]
pub(crate) struct EyeDiagramState {
    /// 2D density grid: `grid[x_bin * grid_y + y_bin]`.
    pub grid: Vec<f64>,
    /// Grid resolution (x, y).
    pub grid_x: usize,
    pub grid_y: usize,
    /// Voltage range of the grid.
    pub y_range: (f64, f64),
    /// Number of overlaid segments.
    pub n_segments: usize,
    /// Total number of samples processed.
    pub n_samples: usize,
    /// Texture needs regeneration.
    pub dirty: bool,
    /// Reset plot view on next render.
    pub reset_view: bool,
    /// Cached egui texture handle.
    pub texture: Option<egui::TextureHandle>,
}

impl EyeDiagramState {
    pub fn new(grid_x: usize, grid_y: usize) -> Self {
        Self {
            grid: vec![0.0; grid_x * grid_y],
            grid_x,
            grid_y,
            y_range: (-1.0, 1.0),
            n_segments: 0,
            n_samples: 0,
            dirty: true,
            reset_view: true,
            texture: None,
        }
    }

    pub fn clear(&mut self) {
        self.grid.fill(0.0);
        self.n_segments = 0;
        self.n_samples = 0;
        self.dirty = true;
        self.texture = None;
    }

    /// Rasterise one waveform segment into the grid.
    ///
    /// Consecutive samples are connected by DDA line rasterisation so
    /// that the resulting traces appear as continuous curves rather
    /// than isolated dots.
    pub fn accumulate_segment(&mut self, segment: &[(f64, f64)], total_width: f64) {
        if segment.len() < 2 || total_width <= 0.0 {
            return;
        }
        let gx = self.grid_x as f64;
        let gy = self.grid_y as f64;
        let y_lo = self.y_range.0;
        let y_hi = self.y_range.1;
        let y_span = y_hi - y_lo;
        if y_span <= 0.0 {
            return;
        }

        // Pre-compute grid coordinates for every sample.
        let coords: Vec<(f64, f64)> = segment
            .iter()
            .map(|&(t, v)| (t / total_width * gx, (v - y_lo) / y_span * gy))
            .collect();

        for win in coords.windows(2) {
            let (fx0, fy0) = win[0];
            let (fx1, fy1) = win[1];

            // Number of DDA steps: at least one step per grid cell
            // along the longer axis so no gaps appear.
            let dx = fx1 - fx0;
            let dy = fy1 - fy0;
            let len = dx.abs().max(dy.abs());
            let n_steps = (len.ceil() as usize).max(1);

            for i in 0..=n_steps {
                let frac = i as f64 / n_steps as f64;
                let fx = fx0 + frac * dx;
                let fy = fy0 + frac * dy;

                self.splat(fx, fy);
            }
        }

        self.n_segments += 1;
        self.n_samples += segment.len();
    }

    /// Bilinear splat of a single point into the density grid.
    #[inline]
    fn splat(&mut self, fx: f64, fy: f64) {
        let x0 = fx.floor() as i64;
        let y0 = fy.floor() as i64;
        let wx = fx - x0 as f64;
        let wy = fy - y0 as f64;

        for (dx, dy, w) in [
            (0i64, 0i64, (1.0 - wx) * (1.0 - wy)),
            (1, 0, wx * (1.0 - wy)),
            (0, 1, (1.0 - wx) * wy),
            (1, 1, wx * wy),
        ] {
            let xi = x0 + dx;
            let yi = y0 + dy;
            if xi >= 0 && xi < self.grid_x as i64 && yi >= 0 && yi < self.grid_y as i64 {
                self.grid[xi as usize * self.grid_y + yi as usize] += w;
            }
        }
    }

    /// Convert the grid to an egui `ColorImage`.
    ///
    /// Uses log-scale normalisation so that both dense trajectories and
    /// sparse regions remain visible despite the large dynamic range.
    pub fn to_color_image(
        &self,
        color_mode: EyeColorMode,
        base_color: Color32,
        saturation: f64,
        _n_ui: usize,
    ) -> egui::ColorImage {
        let gx = self.grid_x;
        let gy = self.grid_y;

        // Log-scale normalisation: compress the enormous dynamic range
        // of a real eye diagram so that trajectories AND the eye
        // interior are both visible.
        let max_val = self
            .grid
            .iter()
            .cloned()
            .fold(0.0_f64, f64::max)
            .max(1e-30);
        let log_max = (1.0 + max_val).ln();

        let pixels: Vec<Color32> = self
            .grid
            .iter()
            .map(|&v| {
                if v <= 0.0 {
                    return Color32::BLACK;
                }
                // Log compression: ln(1+v) / ln(1+max) → [0, 1]
                // Then apply saturation as a gamma-like boost.
                let raw = (1.0 + v).ln() / log_max;
                let t = (raw * saturation).min(1.0) as f32;
                match color_mode {
                    EyeColorMode::Rainbow => rainbow_color(t),
                    EyeColorMode::Monochrome => mono_color(t, base_color),
                    EyeColorMode::Temperature => temperature_color(t),
                    EyeColorMode::Grayscale => grayscale_color(t),
                    EyeColorMode::Phosphor => phosphor_color(t),
                    EyeColorMode::Viridis => viridis_color(t),
                    EyeColorMode::Ironbow => ironbow_color(t),
                    EyeColorMode::CrtAmber => crt_amber_color(t),
                }
            })
            .collect();

        // The grid is stored as [x_bin * gy + y_bin], i.e. column-major.
        // egui::ColorImage expects row-major (row = scanline), so we transpose.
        let mut row_pixels = Vec::with_capacity(gx * gy);
        for y in 0..gy {
            for x in 0..gx {
                row_pixels.push(pixels[x * gy + y]);
            }
        }

        egui::ColorImage {
            size: [gx, gy],
            source_size: egui::Vec2::new(gx as f32, gy as f32),
            pixels: row_pixels,
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
// Colour mapping functions
// =======================================================================

fn mono_color(t: f32, base: Color32) -> Color32 {
    let r = base.r() as f32 / 255.0;
    let g = base.g() as f32 / 255.0;
    let b = base.b() as f32 / 255.0;
    Color32::from_rgb(
        (r * t * 255.0) as u8,
        (g * t * 255.0) as u8,
        (b * t * 255.0) as u8,
    )
}

fn rainbow_color(t: f32) -> Color32 {
    let h = (1.0 - t) * 240.0 / 360.0; // blue → red
    let (r, g, b) = hsv_to_rgb(h, 1.0, t);
    Color32::from_rgb(r, g, b)
}

fn temperature_color(t: f32) -> Color32 {
    // black → red → yellow → white
    if t < 0.33 {
        let s = t / 0.33;
        Color32::from_rgb((s * 255.0) as u8, 0, 0)
    } else if t < 0.66 {
        let s = (t - 0.33) / 0.33;
        Color32::from_rgb(255, (s * 255.0) as u8, 0)
    } else {
        let s = (t - 0.66) / 0.34;
        Color32::from_rgb(255, 255, (s * 255.0) as u8)
    }
}

fn grayscale_color(t: f32) -> Color32 {
    let v = (t * 255.0) as u8;
    Color32::from_rgb(v, v, v)
}

fn phosphor_color(t: f32) -> Color32 {
    // CRT green phosphor: dark → green → bright green
    Color32::from_rgb(
        (t * t * 80.0) as u8,
        (t * 255.0) as u8,
        (t * t * 40.0) as u8,
    )
}

fn viridis_color(t: f32) -> Color32 {
    // Simplified viridis-like colormap (dark purple → teal → yellow)
    let r = if t < 0.5 {
        0.267 + t * 2.0 * (0.004 - 0.267)
    } else {
        0.004 + (t - 0.5) * 2.0 * (0.993 - 0.004)
    };
    let g = if t < 0.5 {
        0.004 + t * 2.0 * (0.373 - 0.004)
    } else {
        0.373 + (t - 0.5) * 2.0 * (0.873 - 0.373)
    };
    let b = if t < 0.5 {
        0.329 + t * 2.0 * (0.471 - 0.329)
    } else {
        0.471 + (t - 0.5) * 2.0 * (0.142 - 0.471)
    };
    Color32::from_rgb((r * 255.0) as u8, (g * 255.0) as u8, (b * 255.0) as u8)
}

fn ironbow_color(t: f32) -> Color32 {
    // Black → red → orange → yellow → white
    if t < 0.25 {
        let s = t / 0.25;
        Color32::from_rgb((s * 128.0) as u8, 0, 0)
    } else if t < 0.5 {
        let s = (t - 0.25) / 0.25;
        Color32::from_rgb((128.0 + s * 127.0) as u8, (s * 165.0) as u8, 0)
    } else if t < 0.75 {
        let s = (t - 0.5) / 0.25;
        Color32::from_rgb(255, (165.0 + s * 90.0) as u8, (s * 64.0) as u8)
    } else {
        let s = (t - 0.75) / 0.25;
        Color32::from_rgb(
            255,
            255,
            (64.0 + s * 191.0) as u8,
        )
    }
}

fn crt_amber_color(t: f32) -> Color32 {
    // CRT amber: dark → warm amber → bright
    Color32::from_rgb(
        (t * 255.0) as u8,
        (t * t * 200.0) as u8,
        (t * t * t * 64.0) as u8,
    )
}

fn hsv_to_rgb(h: f32, s: f32, v: f32) -> (u8, u8, u8) {
    let c = v * s;
    let x = c * (1.0 - ((h * 6.0) % 2.0 - 1.0).abs());
    let m = v - c;
    let (r, g, b) = if h < 1.0 / 6.0 {
        (c, x, 0.0)
    } else if h < 2.0 / 6.0 {
        (x, c, 0.0)
    } else if h < 3.0 / 6.0 {
        (0.0, c, x)
    } else if h < 4.0 / 6.0 {
        (0.0, x, c)
    } else if h < 5.0 / 6.0 {
        (x, 0.0, c)
    } else {
        (c, 0.0, x)
    };
    (
        ((r + m) * 255.0) as u8,
        ((g + m) * 255.0) as u8,
        ((b + m) * 255.0) as u8,
    )
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
        let is_first_open = self.eye_state.is_none();
        if is_first_open {
            self.eye_state = Some(EyeDiagramState::new(
                self.eye_grid_x,
                self.eye_grid_y,
            ));
            // Auto-detect UI and compute in a single pass.
            self.do_auto_detect_and_compute();
            self.eye_prev_bounds = self.last_bounds;
        }

        let eye_ui_period = self.eye_ui_period;
        let eye_channel = self.eye_channel;
        let n_ch = self.data.as_ref().map(|d| d.n_channels()).unwrap_or(0);
        let eye_auto_threshold = self.eye_auto_threshold;
        let eye_n_ui = self.eye_n_ui;
        let total_width = eye_n_ui as f64 * eye_ui_period;
        let prev_color_mode = self.eye_color_mode;

        // Deferred action flags.
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

                    ui.label("Clock:");
                    let clock_label = self
                        .eye_clock_channel
                        .map(|c| {
                            self.channels
                                .get(c)
                                .map(|ch| ch.name.as_str())
                                .unwrap_or("---")
                        })
                        .unwrap_or("Auto (self)");
                    egui::ComboBox::from_id_salt("eye_clk_select")
                        .selected_text(clock_label)
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut self.eye_clock_channel,
                                None,
                                "Auto (self)",
                            );
                            for i in 0..n_ch.min(self.channels.len()) {
                                ui.selectable_value(
                                    &mut self.eye_clock_channel,
                                    Some(i),
                                    &self.channels[i].name,
                                );
                            }
                        });

                    ui.separator();

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
                    let changed = ui
                        .add(
                            egui::TextEdit::singleline(&mut self.eye_ui_period_str)
                                .desired_width(80.0)
                                .hint_text("e.g. 1e-9"),
                        )
                        .lost_focus();
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
                                ui.selectable_value(
                                    &mut self.eye_color_mode,
                                    mode,
                                    mode.to_string(),
                                );
                            }
                        });

                    ui.separator();

                    ui.label("UIs:");
                    let mut n_tmp = self.eye_n_ui as u32;
                    if ui
                        .add(
                            egui::DragValue::new(&mut n_tmp)
                                .range(2..=8)
                                .speed(0.1),
                        )
                        .changed()
                    {
                        self.eye_n_ui = n_tmp as usize;
                        self.eye_needs_recompute = true;
                    }

                    ui.separator();

                    ui.label("Saturation:");
                    let mut sat_tmp = self.eye_saturation;
                    if ui
                        .add(
                            egui::DragValue::new(&mut sat_tmp)
                                .range(0.5..=4.0)
                                .speed(0.05)
                                .custom_formatter(|v, _| format!("{:.1}", v)),
                        )
                        .changed()
                    {
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
                    if ui
                        .button("Auto UI")
                        .on_hover_text(
                            "Detect UI period from signal edges\n(uses Schmitt-trigger + gap clustering)",
                        )
                        .clicked()
                    {
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
                let n_segments = self
                    .eye_state
                    .as_ref()
                    .map(|s| s.n_segments)
                    .unwrap_or(0);
                let n_samples = self
                    .eye_state
                    .as_ref()
                    .map(|s| s.n_samples)
                    .unwrap_or(0);
                ui.horizontal(|ui| {
                    ui.label(format!(
                        "Segments: {}  |  Samples: {}",
                        n_segments, n_samples
                    ));
                    ui.separator();
                    ui.label(format!(
                        "{} UI x {:.4e} s = {:.4e} s",
                        eye_n_ui, eye_ui_period, total_width,
                    ));
                    if eye_auto_threshold {
                        ui.label("(auto)");
                    }
                    ui.separator();
                    ui.label(format!(
                        "Grid: {}x{}  |  Sat: {:.1}",
                        self.eye_grid_x, self.eye_grid_y, self.eye_saturation,
                    ));

                    // Show hint when main view has changed
                    if self.eye_prev_bounds != PlotBounds::NOTHING
                        && self.eye_prev_bounds != self.last_bounds
                    {
                        ui.separator();
                        ui.label(
                            egui::RichText::new("View changed — click Recompute")
                                .color(egui::Color32::YELLOW)
                                .small(),
                        );
                    }
                });

                ui.add_space(4.0);

                // ── Render ──
                let state = self.eye_state.as_mut().unwrap();
                let avail = ui.available_size();
                let plot_h = avail.y - 10.0;

                if avail.x > 10.0 && plot_h > 10.0 {
                    let y_min = state.y_range.0;
                    let y_max = state.y_range.1;
                    let ch_color = self
                        .channels
                        .get(eye_channel)
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
                                plot_ui.set_plot_bounds(PlotBounds::from_min_max(
                                    [0.0, y_min],
                                    [n_ui_f, y_max],
                                ));
                                state.reset_view = false;
                            }

                            if let Some(ref texture) = state.texture {
                                let img = PlotImage::new(
                                    "eye_diagram",
                                    texture,
                                    PlotPoint::new(n_ui_f / 2.0, (y_min + y_max) / 2.0),
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
            // Auto-detect UI period, then recompute in the same pass.
            self.do_auto_detect_and_compute();
            self.eye_prev_bounds = self.last_bounds;
        } else if request_recompute || self.eye_needs_recompute {
            self.do_compute_eye_diagram();
            self.eye_needs_recompute = false;
            self.eye_prev_bounds = self.last_bounds;
        }
    }

    // ===================================================================
    // Auto-detect UI period
    // ===================================================================

    /// Auto-detect UI period using hysteresis edge detection + gap-based
    /// period clustering (inspired by ngscopeclient).
    ///
    /// Algorithm:
    /// 1. Schmitt-trigger edge detection avoids noise doubling.
    /// 2. Inter-crossing periods are sorted; the first ratio gap > 1.5
    ///    separates the true UI cluster from 2xUI, 3xUI runs.
    /// 3. Discard top/bottom 10% and average the rest.
    #[allow(dead_code)]
    fn do_auto_detect_ui_period(&mut self) {
        let vis_x_min = self.last_bounds.min()[0];
        let vis_x_max = self.last_bounds.max()[0];
        let ch = self.eye_channel;
        let points = self
            .data
            .as_mut()
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
                let t_cross =
                    points[i - 1][0] + frac * (points[i][0] - points[i - 1][0]);
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

        // ── 3. Inter-crossing periods -> sort ascending ──
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

        let points = self
            .data
            .as_mut()
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
            self.find_clock_edges(clk_idx, vis_x_min, vis_x_max, &points)
        } else {
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
    // Combined auto-detect + compute (single data fetch)
    // ===================================================================

    /// Auto-detect UI period and compute the eye diagram in one pass,
    /// avoiding the double data fetch of calling them separately.
    fn do_auto_detect_and_compute(&mut self) {
        let vis_x_min = self.last_bounds.min()[0];
        let vis_x_max = self.last_bounds.max()[0];
        let ch_idx = self.eye_channel;
        let n_ui = self.eye_n_ui;

        // ── Fetch raw data once (500k limit) ──
        let points = self
            .data
            .as_mut()
            .map(|d| d.get_raw_points(ch_idx, vis_x_min, vis_x_max, 500_000))
            .unwrap_or_default();

        if points.len() < 10 {
            self.status_message = "Not enough data for eye diagram".to_owned();
            return;
        }

        // ── Sort voltages for both y-range and threshold detection ──
        let mut volts: Vec<f64> = points.iter().map(|p| p[1]).collect();
        volts.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let n_volts = volts.len();
        let v_q25 = volts[n_volts / 4];
        let v_q75 = volts[3 * n_volts / 4];
        let v_swing = v_q75 - v_q25;

        // ── Auto-detect UI period from Schmitt-trigger crossings ──
        let threshold = (v_q25 + v_q75) / 2.0;
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
                let t_cross =
                    points[i - 1][0] + frac * (points[i][0] - points[i - 1][0]);
                crossings.push(t_cross);
                is_high = true;
            } else if is_high && curr < thresh_lo {
                is_high = false;
            }
        }

        // Gap-based UI period clustering
        if crossings.len() >= 3 {
            let mut periods: Vec<f64> = crossings
                .windows(2)
                .map(|w| w[1] - w[0])
                .filter(|&dt| dt > 0.0)
                .collect();
            periods.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

            if !periods.is_empty() {
                let mut gap_end = periods.len();
                for i in 1..periods.len() {
                    let ratio = periods[i] / periods[i - 1].max(1e-30);
                    if ratio > 1.5 {
                        gap_end = i;
                        break;
                    }
                }
                let cluster = &periods[..gap_end];
                let count = cluster.len();
                let lo = count / 10;
                let hi = count * 9 / 10;
                let ui_period = if hi > lo {
                    let total: f64 = cluster[lo..=hi].iter().sum();
                    total / (hi - lo + 1) as f64
                } else {
                    cluster[cluster.len() / 2]
                };

                self.eye_ui_period = ui_period;
                self.eye_ui_period_str = format!("{:.6e}", ui_period);
                self.eye_auto_threshold = true;
                self.eye_n_ui = 3;
                self.status_message = format!(
                    "Auto-detected UI: {:.4e} s ({:.2} MHz)",
                    ui_period,
                    1.0 / ui_period / 1e6,
                );
            }
        }

        // ── Compute eye diagram from the same data ──
        let ui_period = self.eye_ui_period;
        if ui_period <= 0.0 || n_ui < 2 {
            return;
        }
        let total_width = n_ui as f64 * ui_period;

        // Y range with 5% padding
        let y_lo = volts[0];
        let y_hi = volts[n_volts - 1];
        let margin = (y_hi - y_lo) * 0.05;
        let y_range = (y_lo - margin, y_hi + margin);

        // Reset state
        if let Some(ref mut state) = self.eye_state {
            state.clear();
            state.y_range = y_range;
        }

        // Find clock edges (reuse the already-fetched data)
        let vis_x_min = self.last_bounds.min()[0];
        let vis_x_max = self.last_bounds.max()[0];
        let trigger_points = if let Some(clk_idx) = self.eye_clock_channel {
            self.find_clock_edges(clk_idx, vis_x_min, vis_x_max, &points)
        } else {
            self.find_signal_edges(&points, &volts)
        };

        // Fallback: fixed-period segmentation
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

        // Accumulate segments
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
                        _ => {
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
        &mut self,
        clk_idx: usize,
        vis_x_min: f64,
        vis_x_max: f64,
        signal_points: &[[f64; 2]],
    ) -> Vec<usize> {
        let clk_points = self
            .data
            .as_mut()
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

        // Map clock edge times -> nearest signal sample indices
        let mut result: Vec<usize> = Vec::new();
        let mut sig_idx: usize = 0;

        for &edge_t in &edge_times {
            while sig_idx + 1 < signal_points.len()
                && signal_points[sig_idx][0] < edge_t
            {
                sig_idx += 1;
            }
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
