//! Main application state and UI rendering.
//!
//! CSV-only static waveform viewer backed by Polars. Each data channel occupies
//! a vertical strip with independently adjustable height. Drag a channel label
//! onto another strip to merge them. All strips share a linked x-axis for
//! synchronised zoom (scroll) and pan (drag). Zoom-aware min/max downsampling
//! keeps interactions fast even with 100 M+ rows.

mod central;
mod cursor_lines;
mod eye_diagram;
mod export_actions;
mod fft_window;
mod interaction;
mod math_dialog;
mod measurement_panel;
mod toolbar;
mod xy_window;

use egui::Color32;
use egui_plot::PlotBounds;

use crate::cursor::CursorState;
use crate::data::WaveformData;
use crate::fft_analysis;
use crate::math_channel::{MathChannelDef, MathOp};
use crate::measurement::Measurements;

use eye_diagram::{ClockPolarity, EyeColorMode, EyeDiagramState};

// ---------- constants ----------

pub(crate) const CHANNEL_COLORS: [Color32; 8] = [
    Color32::from_rgb(0, 200, 255),
    Color32::from_rgb(255, 200, 0),
    Color32::from_rgb(0, 255, 100),
    Color32::from_rgb(255, 100, 100),
    Color32::from_rgb(200, 130, 255),
    Color32::from_rgb(255, 160, 60),
    Color32::from_rgb(100, 255, 255),
    Color32::from_rgb(255, 100, 200),
];

/// Maximum display points per channel (per zoom frame).
pub(crate) const MAX_DISPLAY_POINTS: usize = 4000;

/// Minimum strip plot height in pixels.
pub(crate) const MIN_STRIP_HEIGHT: f32 = 60.0;

/// Screen-pixel proximity threshold for cursor drag detection.
pub(crate) const CURSOR_HIT_PX: f32 = 6.0;

// ---------- data model ----------

#[derive(Clone)]
pub(crate) struct ChannelState {
    pub name: String,
    pub visible: bool,
    pub delay: f64,
    pub color: Color32,
    /// Whether the voltage threshold reference line is shown for this channel.
    pub threshold_enabled: bool,
    /// The voltage threshold value (in Volts).
    pub threshold_value: f64,
    /// Whether the binarized (square wave) view is shown for this channel.
    pub binarize_enabled: bool,
    /// When binarize is active, hide the original analog waveform.
    pub binarize_hide_original: bool,
    /// String buffer for the threshold text input.
    pub threshold_text: String,
}

#[derive(Clone)]
pub(crate) struct Strip {
    pub channel_indices: Vec<usize>,
    pub height: f32,
}

#[derive(Clone, Copy)]
pub(crate) struct DragPayload {
    pub channel_idx: usize,
    pub source_strip: usize,
}

/// Cached downsampled points for one channel, keyed by visible x-range.
#[derive(Clone)]
pub(crate) struct StripCache {
    pub points: Vec<[f64; 2]>,
    pub vis_x_min: f64,
    pub vis_x_max: f64,
    pub delay: f64,
    pub ch_idx: usize,
}

impl StripCache {
    pub fn is_valid(&self, vis_x_min: f64, vis_x_max: f64, delay: f64, ch_idx: usize) -> bool {
        self.ch_idx == ch_idx
            && self.delay == delay
            && (self.vis_x_min - vis_x_min).abs() < f64::EPSILON
            && (self.vis_x_max - vis_x_max).abs() < f64::EPSILON
    }
}

// ---------- app struct ----------

pub struct OscilloscopeApp {
    pub(crate) channels: Vec<ChannelState>,
    pub(crate) strips: Vec<Strip>,

    /// Polars-backed waveform data.
    pub(crate) data: Option<WaveformData>,

    /// Per-channel downsample cache.
    pub(crate) cache: Vec<Option<StripCache>>,

    /// Last-known visible x-range, shared across all strips.
    pub(crate) last_bounds: PlotBounds,

    /// True until the first render after loading data.
    pub(crate) needs_initial_fit: bool,

    /// True when undo-zoom or goto-time needs manual bounds restoration.
    pub(crate) needs_undo_zoom: bool,

    /// Channel currently being renamed (inline edit), if any.
    pub(crate) editing_channel: Option<usize>,

    /// File path display.
    pub(crate) loaded_path: String,

    pub(crate) status_message: String,

    // --- Navigation ---
    /// Zoom history stack for undo.
    pub(crate) zoom_history: Vec<PlotBounds>,
    /// User input for "Go to time".
    pub(crate) goto_time_input: String,

    // --- Measurement ---
    /// Which channel to show in the measurement panel.
    pub(crate) measurement_channel: usize,
    /// Per-channel cached measurements + the bounds they were computed for.
    pub(crate) measurement_cache: Vec<Option<(PlotBounds, Measurements)>>,
    /// Toggle visibility of the measurement side-panel.
    pub(crate) show_measurement_panel: bool,
    /// Show measurement values overlaid on waveform plots.
    pub(crate) show_overlay_measurements: bool,
    /// When true + cursor active, restrict measurements to cursor A-B range.
    pub(crate) measurement_gate: bool,

    // --- Cursor ---
    pub(crate) cursor: CursorState,

    // --- Export ---
    pub(crate) pending_screenshot_path: Option<String>,

    // --- FFT ---
    pub(crate) show_fft_window: bool,
    pub(crate) fft_channel: usize,
    pub(crate) fft_window_type: fft_analysis::WindowType,
    pub(crate) fft_scale: fft_analysis::FftScale,
    pub(crate) fft_cache: Option<(
        PlotBounds,
        usize,
        fft_analysis::WindowType,
        fft_analysis::FftScale,
        Vec<[f64; 2]>,
    )>,

    // --- Math channels ---
    pub(crate) math_channels: Vec<MathChannelDef>,

    // --- XY mode ---
    pub(crate) show_xy_window: bool,
    pub(crate) xy_ch_x: usize,
    pub(crate) xy_ch_y: usize,

    // --- Eye diagram ---
    pub(crate) show_eye_window: bool,
    pub(crate) eye_channel: usize,
    pub(crate) eye_clock_channel: Option<usize>,
    pub(crate) eye_clock_polarity: ClockPolarity,
    pub(crate) eye_ui_period: f64,
    pub(crate) eye_ui_period_str: String,
    pub(crate) eye_auto_threshold: bool,
    pub(crate) eye_n_ui: usize,
    pub(crate) eye_color_mode: EyeColorMode,
    pub(crate) eye_grid_x: usize,
    pub(crate) eye_grid_y: usize,
    pub(crate) eye_saturation: f64,
    pub(crate) eye_needs_recompute: bool,
    pub(crate) eye_state: Option<EyeDiagramState>,
    /// Previous visible bounds when eye diagram was last computed.
    pub(crate) eye_prev_bounds: PlotBounds,

    // --- Math dialog ---
    pub(crate) show_math_dialog: bool,
    pub(crate) math_new_op: MathOp,
    pub(crate) math_new_src_a: usize,
    pub(crate) math_new_src_b: usize,
    pub(crate) pending_math_remove: Option<usize>,

    // --- Async file dialog channels (macOS crash workaround) ---
    /// Receiver for open-file dialog results.
    pub(crate) open_file_rx: std::sync::mpsc::Receiver<String>,
    /// Receiver for save-file dialog results (label, path).
    pub(crate) save_file_rx: std::sync::mpsc::Receiver<(String, String)>,

    // --- Background data loading ---
    /// Receiver for background-loaded WaveformData results.
    pub(crate) data_load_rx: std::sync::mpsc::Receiver<Result<WaveformData, String>>,
    /// Path currently being loaded in background (for progress display).
    pub(crate) loading_path: Option<String>,
    /// Loading progress: (rows_scanned, bytes_read, total_bytes).
    pub(crate) loading_progress: Option<(u64, u64, u64)>,
    /// Receiver for loading progress updates from background thread.
    pub(crate) loading_progress_rx: Option<std::sync::mpsc::Receiver<(u64, u64, u64)>>,

    /// Path to load on the first frame (set via command-line argument).
    pub(crate) pending_load_path: Option<String>,
}

impl Default for OscilloscopeApp {
    fn default() -> Self {
        Self {
            channels: Vec::new(),
            strips: Vec::new(),
            data: None,
            cache: Vec::new(),
            last_bounds: PlotBounds::NOTHING,
            needs_initial_fit: false,
            needs_undo_zoom: false,
            editing_channel: None,
            loaded_path: String::new(),
            status_message: "No data loaded — click \"Open CSV...\" to import".to_owned(),

            zoom_history: Vec::new(),
            goto_time_input: String::new(),

            measurement_channel: 0,
            measurement_cache: Vec::new(),
            show_measurement_panel: true,
            show_overlay_measurements: false,
            measurement_gate: false,

            cursor: CursorState::default(),
            pending_screenshot_path: None,

            show_fft_window: false,
            fft_channel: 0,
            fft_window_type: fft_analysis::WindowType::Hanning,
            fft_scale: fft_analysis::FftScale::Db,
            fft_cache: None,

            math_channels: Vec::new(),

            show_xy_window: false,
            xy_ch_x: 0,
            xy_ch_y: 1,

            show_eye_window: false,
            eye_channel: 0,
            eye_clock_channel: None,
            eye_clock_polarity: ClockPolarity::Rising,
            eye_ui_period: 1e-9,
            eye_ui_period_str: "1e-9".to_owned(),
            eye_auto_threshold: false,
            eye_n_ui: 3,
            eye_color_mode: EyeColorMode::Phosphor,
            eye_grid_x: 512,
            eye_grid_y: 256,
            eye_saturation: 2.0,
            eye_needs_recompute: false,
            eye_state: None,
            eye_prev_bounds: PlotBounds::NOTHING,

            show_math_dialog: false,
            math_new_op: MathOp::Add,
            math_new_src_a: 0,
            math_new_src_b: 1,
            pending_math_remove: None,

            // Async file dialog channels
            open_file_rx: std::sync::mpsc::channel().1,
            save_file_rx: std::sync::mpsc::channel::<(String, String)>().1,

            // Background data loading
            data_load_rx: std::sync::mpsc::channel().1,
            loading_path: None,
            loading_progress: None,
            loading_progress_rx: None,
            pending_load_path: None,
        }
    }
}

// ---------- data loading & cache ----------

impl OscilloscopeApp {
    /// Start loading a CSV file in a background thread.
    /// The UI will show a progress message until loading completes.
    pub(crate) fn load_csv_from_path(&mut self, path: &str) {
        self.status_message = format!("Loading {}...", path);
        self.loading_path = Some(path.to_owned());
        self.loading_progress = None;

        let (tx, rx) = std::sync::mpsc::channel();
        self.data_load_rx = rx;

        // Progress channel: (rows, bytes, total)
        let (prog_tx, prog_rx) = std::sync::mpsc::channel::<(u64, u64, u64)>();
        self.loading_progress_rx = Some(prog_rx);

        let path_owned = path.to_owned();
        std::thread::spawn(move || {
            let result = WaveformData::load_csv(&path_owned, &move |rows, bytes, total| {
                let _ = prog_tx.send((rows as u64, bytes, total));
            });
            let _ = tx.send(result);
        });
    }

    /// Called each frame to check if background loading has completed.
    fn check_data_loaded(&mut self) {
        // Consume progress updates
        if let Some(ref prog_rx) = self.loading_progress_rx {
            while let Ok((rows, bytes, total)) = prog_rx.try_recv() {
                self.loading_progress = Some((rows, bytes, total));
            }
        }

        if let Ok(result) = self.data_load_rx.try_recv() {
            self.loading_path = None;
            self.loading_progress = None;
            self.loading_progress_rx = None;
            match result {
                Ok(wd) => {
                    let n_data = wd.n_channels();
                    self.loaded_path = "loaded".to_owned();
                    self.data = Some(wd);

                    self.channels = (0..n_data)
                        .map(|i| ChannelState {
                            name: format!("CH{}", i + 1),
                            visible: true,
                            delay: 0.0,
                            color: CHANNEL_COLORS[i % CHANNEL_COLORS.len()],
                            threshold_enabled: false,
                            threshold_value: 0.0,
                            binarize_enabled: false,
                            binarize_hide_original: false,
                            threshold_text: "0.0".to_owned(),
                        })
                        .collect();

                    self.strips = (0..n_data)
                        .map(|_| Strip {
                            channel_indices: vec![0],
                            height: 150.0,
                        })
                        .collect();
                    for (i, s) in self.strips.iter_mut().enumerate() {
                        s.channel_indices = vec![i];
                    }

                    self.cache = vec![None; n_data];
                    self.measurement_cache = vec![None; n_data];
                    self.measurement_channel = 0;
                    self.needs_initial_fit = true;

                    let wd = self.data.as_ref().unwrap();
                    self.last_bounds =
                        PlotBounds::from_min_max([wd.x_min(), -1.0], [wd.x_max(), 1.0]);

                    self.cursor = CursorState::default();

                    self.status_message = format!(
                        "Loaded {} channels × {} rows — scroll to zoom, drag to pan",
                        n_data, wd.n_rows(),
                    );
                }
                Err(e) => {
                    self.status_message = format!("Error: {}", e);
                }
            }
        }
    }

    /// Ensure the downsample cache for one channel is up-to-date.
    pub(crate) fn ensure_cache(&mut self, ch_idx: usize) {
        let Some(ref data) = self.data else { return };
        let n_real = data.n_channels();

        // Delegate math channels
        if ch_idx >= n_real {
            self.ensure_math_cache(ch_idx);
            return;
        }

        if ch_idx >= self.channels.len() || ch_idx >= self.cache.len() {
            return;
        }

        let vis_x_min = self.last_bounds.min()[0];
        let vis_x_max = self.last_bounds.max()[0];
        let delay = self.channels[ch_idx].delay;

        if let Some(ref c) = self.cache[ch_idx] {
            if c.is_valid(vis_x_min, vis_x_max, delay, ch_idx) {
                return;
            }
        }

        let points = data.get_channel_points(
            ch_idx,
            delay,
            vis_x_min,
            vis_x_max,
            MAX_DISPLAY_POINTS,
        );

        self.cache[ch_idx] = Some(StripCache {
            points,
            vis_x_min,
            vis_x_max,
            delay,
            ch_idx,
        });
    }

    /// Ensure measurement cache is valid for the selected channel.
    ///
    /// When `measurement_gate` is enabled and vertical cursor is active,
    /// measurements are restricted to the cursor A-B range.
    pub(crate) fn ensure_measurements(&mut self, ch_idx: usize) {
        let Some(ref _data) = self.data else { return };
        if ch_idx >= self.channels.len() || ch_idx >= self.measurement_cache.len() {
            return;
        }

        let meas_range = self.measurement_range();
        let vis_x_min = meas_range.0;
        let vis_x_max = meas_range.1;

        let needs_compute = match &self.measurement_cache[ch_idx] {
            Some((bounds, _)) => {
                (bounds.min()[0] - vis_x_min).abs() > f64::EPSILON
                    || (bounds.max()[0] - vis_x_max).abs() > f64::EPSILON
            }
            None => true,
        };

        if needs_compute {
            let data = self.data.as_ref().unwrap();
            let m = Measurements::compute(data, ch_idx, vis_x_min, vis_x_max);
            // Store with the actual measurement range as cache key
            self.measurement_cache[ch_idx] = Some((
                egui_plot::PlotBounds::from_min_max([vis_x_min, 0.0], [vis_x_max, 0.0]),
                m,
            ));
        }
    }

    /// Get the effective measurement range (full visible or gated by cursor).
    pub(crate) fn measurement_range(&self) -> (f64, f64) {
        use crate::cursor::CursorMode;
        let vis_x_min = self.last_bounds.min()[0];
        let vis_x_max = self.last_bounds.max()[0];
        if self.measurement_gate
            && self.cursor.mode == CursorMode::Vertical
        {
            let a = self.cursor.pos_a.min(self.cursor.pos_b);
            let b = self.cursor.pos_a.max(self.cursor.pos_b);
            // Clamp to visible range
            (a.max(vis_x_min), b.min(vis_x_max))
        } else {
            (vis_x_min, vis_x_max)
        }
    }
}

// ---------- navigation ----------

impl OscilloscopeApp {
    /// Push current bounds to zoom history (call before changing bounds).
    pub(crate) fn push_zoom_history(&mut self) {
        if self.last_bounds != PlotBounds::NOTHING {
            self.zoom_history.push(self.last_bounds);
            // Keep a reasonable max
            if self.zoom_history.len() > 50 {
                self.zoom_history.remove(0);
            }
        }
    }

    /// Restore previous zoom level from history.
    pub(crate) fn undo_zoom(&mut self) {
        if let Some(prev) = self.zoom_history.pop() {
            self.last_bounds = prev;
            self.needs_undo_zoom = true;
            self.status_message = "Zoom restored".to_owned();
        }
    }

    /// Jump to a specific time value entered by the user.
    pub(crate) fn goto_time(&mut self) {
        let input = self.goto_time_input.trim();
        if input.is_empty() {
            return;
        }
        if let Ok(target_t) = input.parse::<f64>() {
            let x_span = self.last_bounds.max()[0] - self.last_bounds.min()[0];
            let half = x_span / 2.0;
            self.push_zoom_history();
            self.last_bounds = PlotBounds::from_min_max(
                [target_t - half, self.last_bounds.min()[1]],
                [target_t + half, self.last_bounds.max()[1]],
            );
            self.needs_undo_zoom = true;
            self.status_message =
                format!("Jumped to t = {}", Measurements::format_value(target_t, "s"));
        } else {
            self.status_message = "Invalid time value".to_owned();
        }
    }
}

// ---------- eframe::App ----------

impl eframe::App for OscilloscopeApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Handle command-line file path on first frame
        if let Some(path) = self.pending_load_path.take() {
            self.load_csv_from_path(&path);
        }

        // Check if background data loading finished
        self.check_data_loaded();

        self.check_screenshot_events(ctx);
        self.draw_toolbar(ctx);

        // Show loading overlay while data is being read
        if self.loading_path.is_some() {
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space(ui.available_height() / 3.0);
                    ui.spinner();
                    ui.add_space(8.0);

                    let file_name = self.loading_path.as_deref()
                        .and_then(|p| p.rsplit('/').next())
                        .unwrap_or("file");

                    if let Some((rows, bytes, total)) = self.loading_progress {
                        let pct = if total > 0 { bytes * 100 / total } else { 0 };
                        let mb_read = bytes as f64 / 1e6;
                        let mb_total = total as f64 / 1e6;
                        ui.label(
                            egui::RichText::new(format!(
                                "Indexing {} — {}%",
                                file_name, pct.min(100),
                            ))
                            .size(18.0),
                        );
                        ui.label(
                            egui::RichText::new(format!(
                                "{:.0} MB / {:.0} MB  •  {:.0} M rows scanned",
                                mb_read, mb_total, rows as f64 / 1e6,
                            ))
                            .small()
                            .color(egui::Color32::GRAY),
                        );
                        // Progress bar
                        let progress = if total > 0 { bytes as f32 / total as f32 } else { 0.0 };
                        ui.add(egui::ProgressBar::new(progress.min(1.0)).show_percentage());
                    } else {
                        ui.label(
                            egui::RichText::new(format!("Loading {}…", file_name))
                                .size(18.0),
                        );
                        ui.label(
                            egui::RichText::new("Parsing CSV file…")
                                .small()
                                .color(egui::Color32::GRAY),
                        );
                    }
                });
            });
            ctx.request_repaint();
            return;
        }

        if self.show_measurement_panel && self.data.is_some() {
            self.draw_measurement_panel(ctx);
        }
        self.draw_central(ctx);
        if self.show_fft_window {
            self.draw_fft_window(ctx);
        }
        if self.show_xy_window {
            self.draw_xy_window(ctx);
        }
        if self.show_eye_window {
            self.draw_eye_diagram(ctx);
        }
        self.draw_math_dialog(ctx);
    }
}
