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
/// Max points per channel fed to egui_plot::Line. GPU rasterizes these
/// effortlessly; this is bounded to keep memory and tessellation in check.
/// 200k gives good waveform detail even at full range on large files.
pub(crate) const MAX_DISPLAY_POINTS: usize = 200_000;

/// Minimum strip plot height in pixels.
pub(crate) const MIN_STRIP_HEIGHT: f32 = 60.0;

/// Screen-pixel proximity threshold for cursor drag detection.
pub(crate) const CURSOR_HIT_PX: f32 = 6.0;

// ---------- data model ----------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum TimeUnit {
    Ps,
    Ns,
    Us,
    Ms,
    S,
}

impl TimeUnit {
    pub fn suffix(&self) -> &'static str {
        match self {
            TimeUnit::Ps => "ps",
            TimeUnit::Ns => "ns",
            TimeUnit::Us => "μs",
            TimeUnit::Ms => "ms",
            TimeUnit::S => "s",
        }
    }

    /// Convert a value in seconds to this unit.
    pub fn from_seconds(&self, s: f64) -> f64 {
        match self {
            TimeUnit::Ps => s * 1e12,
            TimeUnit::Ns => s * 1e9,
            TimeUnit::Us => s * 1e6,
            TimeUnit::Ms => s * 1e3,
            TimeUnit::S => s,
        }
    }

    /// Convert a value in this unit to seconds.
    pub fn to_seconds(&self, v: f64) -> f64 {
        match self {
            TimeUnit::Ps => v * 1e-12,
            TimeUnit::Ns => v * 1e-9,
            TimeUnit::Us => v * 1e-6,
            TimeUnit::Ms => v * 1e-3,
            TimeUnit::S => v,
        }
    }

    pub fn all() -> &'static [TimeUnit] {
        &[TimeUnit::Ps, TimeUnit::Ns, TimeUnit::Us, TimeUnit::Ms, TimeUnit::S]
    }
}

impl std::fmt::Display for TimeUnit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.suffix())
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum VoltageUnit {
    V,
    Mv,
}

impl VoltageUnit {
    pub fn suffix(&self) -> &'static str {
        match self {
            VoltageUnit::V => "V",
            VoltageUnit::Mv => "mV",
        }
    }

    /// Convert a value in Volts to this unit.
    pub fn from_volts(&self, v: f64) -> f64 {
        match self {
            VoltageUnit::V => v,
            VoltageUnit::Mv => v * 1e3,
        }
    }

    /// Convert a value in this unit to Volts.
    pub fn to_volts(&self, v: f64) -> f64 {
        match self {
            VoltageUnit::V => v,
            VoltageUnit::Mv => v * 1e-3,
        }
    }

    pub fn all() -> &'static [VoltageUnit] {
        &[VoltageUnit::V, VoltageUnit::Mv]
    }
}

impl std::fmt::Display for VoltageUnit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.suffix())
    }
}

#[derive(Clone)]
pub(crate) struct ChannelState {
    pub name: String,
    pub visible: bool,
    pub delay: f64,
    pub delay_unit: TimeUnit,
    pub color: Color32,
    pub threshold_enabled: bool,
    pub threshold_value: f64,
    pub threshold_unit: VoltageUnit,
    pub binarize_enabled: bool,
    pub binarize_hide_original: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum YAxisMode {
    /// All strips share the same Y range (linked).
    Linked,
    /// Each strip auto-adjusts its own Y range.
    Auto,
    /// User manually sets Y min/max per strip.
    Manual,
}

#[derive(Clone)]
pub(crate) struct Strip {
    pub channel_indices: Vec<usize>,
    pub height: f32,
    pub y_mode: YAxisMode,
    pub y_min: f64,
    pub y_max: f64,
    /// Vertical offset for Linked mode: shifts the waveform up/down per strip.
    pub y_offset: f64,
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
    /// True when points came from per-chunk statistics (coarse preview).
    /// Background decode replaces this with detailed envelope data.
    pub is_coarse: bool,
}

impl StripCache {
    pub fn is_valid(&self, vis_x_min: f64, vis_x_max: f64, delay: f64, ch_idx: usize) -> bool {
        if self.ch_idx != ch_idx || self.delay != delay {
            return false;
        }
        // Visible range must be within the over-fetched cached range so
        // panning within the buffer doesn't trigger a new decode.
        if !(self.vis_x_min <= vis_x_min && self.vis_x_max >= vis_x_max) {
            return false;
        }
        // Reject if cached resolution is too low (significant zoom-in since
        // the cache was populated).
        let cached_span = self.vis_x_max - self.vis_x_min;
        let vis_span = (vis_x_max - vis_x_min).max(1e-30);
        cached_span / vis_span <= 4.0
    }
}

// ---------- app struct ----------

pub struct OscilloscopeApp {
    pub(crate) channels: Vec<ChannelState>,
    pub(crate) strips: Vec<Strip>,

    /// Global Y-axis range for Linked mode: center and half-span.
    pub(crate) y_linked_center: f64,
    pub(crate) y_linked_half_span: f64,

    /// Polars-backed waveform data.
    pub(crate) data: Option<WaveformData>,

    /// Per-channel downsample cache.
    pub(crate) cache: Vec<Option<StripCache>>,

    /// Last-known visible x-range, shared across all strips.
    pub(crate) last_bounds: PlotBounds,

    /// True until the first render after loading data.
    pub(crate) needs_initial_fit: bool,
    /// Whether the initial Y auto-fit has run after the first background
    /// decode completed. Reset on each new file load.
    pub(crate) y_auto_fitted: bool,

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
    pub(crate) goto_time_unit: TimeUnit,

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

    // --- Background line-cache decode ---
    /// Receiver for background-decoded line points.
    pub(crate) cache_rx: Option<std::sync::mpsc::Receiver<CacheDecodeResult>>,
    /// Bounds the in-flight background decode targets. Non-None ⇒ a decode is
    /// running; new requests are dropped until it finishes.
    pub(crate) cache_inflight_bounds: Option<(f64, f64)>,

    // --- Density rendering ---
    pub(crate) density_mode: bool,
    pub(crate) density_caches: Vec<crate::density_renderer::DensityCache>,
 }

/// Result of a background line-cache decode pass.
pub(crate) struct CacheDecodeResult {
    pub channels: Vec<usize>,
    pub points: Vec<Vec<[f64; 2]>>,
    pub vis_x_min: f64,
    pub vis_x_max: f64,
    pub delays: Vec<f64>,
}

impl Default for OscilloscopeApp {
    fn default() -> Self {
        Self {
            channels: Vec::new(),
            strips: Vec::new(),
            y_linked_center: 0.0,
            y_linked_half_span: 1.0,
            data: None,
            cache: Vec::new(),
            last_bounds: PlotBounds::NOTHING,
            needs_initial_fit: false,
            y_auto_fitted: false,
            needs_undo_zoom: false,
            editing_channel: None,
            loaded_path: String::new(),
            status_message: "No data loaded — click \"Open CSV...\" to import".to_owned(),

            zoom_history: Vec::new(),
            goto_time_input: String::new(),
            goto_time_unit: TimeUnit::Ps,

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

            // Background line-cache decode
            cache_rx: None,
            cache_inflight_bounds: None,

            density_mode: false,
            density_caches: Vec::new(),
        }
    }
}

// ---------- data loading & cache ----------

impl OscilloscopeApp {
    /// Set a path to load on the first frame (for command-line arguments).
    pub fn set_pending_load_path(&mut self, path: String) {
        self.pending_load_path = Some(path);
    }

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
            // Capture total CSV size before clearing progress (used to cap the
            // initial view range for very large files).
            let csv_total_bytes = self.loading_progress.and_then(|(_, _, t)| {
                if t > 0 { Some(t) } else { None }
            });
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
                            delay_unit: TimeUnit::Ps,
                            color: CHANNEL_COLORS[i % CHANNEL_COLORS.len()],
                            threshold_enabled: false,
                            threshold_value: 0.0,
                            threshold_unit: VoltageUnit::V,
                            binarize_enabled: false,
                            binarize_hide_original: false,
                        })
                        .collect();

                    self.strips = (0..n_data)
                        .map(|_| Strip {
                            channel_indices: vec![0],
                            height: 150.0,
                            y_mode: YAxisMode::Linked,
                            y_min: -1.0,
                            y_max: 1.0,
                            y_offset: 0.0,
                        })
                        .collect();
                    for (i, s) in self.strips.iter_mut().enumerate() {
                        s.channel_indices = vec![i];
                    }

                    self.cache = vec![None; n_data];
                    self.density_caches = (0..n_data)
                        .map(|_| crate::density_renderer::DensityCache::new())
                        .collect();
                    self.measurement_cache = vec![None; n_data];
                    self.measurement_channel = 0;
                    self.needs_initial_fit = true;
                    self.y_auto_fitted = false;

                    let wd = self.data.as_ref().unwrap();

                    // For very large files, showing the full range on first
                    // paint would force the background decode to read every
                    // TSZ chunk (thousands for a 30 GB file). Cap the initial
                    // view instead; the rest loads on demand via pan/zoom.
                    const INITIAL_VIEW_BYTES: u64 = 4 * 1024 * 1024 * 1024; // 4 GB
                    let x_lo = wd.x_min();
                    let span = wd.time_span();
                    let ratio = match csv_total_bytes {
                        Some(total) if total > INITIAL_VIEW_BYTES => {
                            INITIAL_VIEW_BYTES as f64 / total as f64
                        }
                        _ => 1.0,
                    };
                    let x_hi = x_lo + span * ratio.max(1e-6);

                    self.last_bounds =
                        PlotBounds::from_min_max([x_lo, -1.0], [x_hi, 1.0]);

                    self.cursor = CursorState::default();

                    if ratio < 0.999 {
                        self.status_message = format!(
                            "Loaded {} ch × {} rows — showing {:.0}% (pan/zoom to load more)",
                            n_data, wd.n_rows(), ratio * 100.0,
                        );
                    } else {
                        self.status_message = format!(
                            "Loaded {} channels × {} rows — scroll to zoom, drag to pan",
                            n_data, wd.n_rows(),
                        );
                    }
                }
                Err(e) => {
                    self.status_message = format!("Error: {}", e);
                }
            }
        }
    }

    /// Synchronous cache refresh — used only for **math channels** (derived
    /// from other channels' cached points). Real channels go through the
    /// background `ensure_cache_async` path.
    pub(crate) fn ensure_cache(&mut self, ch_idx: usize) {
        let Some(ref data) = self.data else { return };
        if ch_idx < data.n_channels() {
            return; // real channel: handled by ensure_cache_async
        }
        self.ensure_math_cache(ch_idx);
    }

    /// Ensure line-mode downsample caches for the given channels.
    ///
    /// Decoding runs on a **background thread** (stateless, no LRU contention
    /// with the UI thread's `ChunkStore`), so the UI never blocks on TSZ
    /// decode. While a decode is in flight the affected channels keep their
    /// previous (or empty) cache; the result is consumed in
    /// `finish_cache_async` on a later frame.
    pub(crate) fn ensure_cache_async(&mut self, ctx: &egui::Context, channels: &[usize]) {
        let Some(ref data) = self.data else { return };

        // While ANY decode is in flight, never spawn another.
        if self.cache_inflight_bounds.is_some() {
            return;
        }

        let vis_x_min = self.last_bounds.min()[0];
        let vis_x_max = self.last_bounds.max()[0];

        // Over-fetch: decode 2× the visible range (50% margin each side) so
        // small pans stay within the cached range without a new decode.
        let vis_span = vis_x_max - vis_x_min;
        let margin = vis_span * 0.5;
        let decode_x_min = vis_x_min - margin;
        let decode_x_max = vis_x_max + margin;
        // Scale max_points proportionally to keep the same point density.
        let decode_max_points = MAX_DISPLAY_POINTS * 2;

        // Collect real channels whose cache is stale (math channels handled
        // separately by ensure_cache, which stays synchronous).
        let n_real = data.n_channels();
        let stale: Vec<usize> = channels
            .iter()
            .copied()
            .filter(|&ch_idx| {
                if ch_idx >= n_real || ch_idx >= self.channels.len() || ch_idx >= self.cache.len() {
                    return false;
                }
                let delay = self.channels[ch_idx].delay;
                match self.cache.get(ch_idx).and_then(|c| c.as_ref()) {
                    Some(cached) => !cached.is_valid(vis_x_min, vis_x_max, delay, ch_idx),
                    None => true,
                }
            })
            .collect();

        if stale.is_empty() {
            return;
        }

        let decode_max_points =
            (1_500_000 / stale.len()).clamp(20_000, decode_max_points);

        let chunks_dir = data.chunks_dir().to_path_buf();
        let entries: Vec<_> = data.entries().to_vec();
        let delays: Vec<f64> = stale
            .iter()
            .map(|&ch| self.channels[ch].delay)
            .collect();
        let (tx, rx) = std::sync::mpsc::channel();
        self.cache_rx = Some(rx);
        self.cache_inflight_bounds = Some((decode_x_min, decode_x_max));

        let req_chs = stale.clone();
        let req_vis_min = decode_x_min;
        let req_vis_max = decode_x_max;

        std::thread::spawn(move || {
            let raw = crate::data::chunk_store::read_raw_points_multi_readonly(
                &chunks_dir,
                &entries,
                &req_chs,
                req_vis_min,
                req_vis_max,
                decode_max_points,
            );
            // Apply per-channel delay to the x coordinate (display convention).
            let points: Vec<Vec<[f64; 2]>> = raw
                .into_iter()
                .zip(delays.iter())
                .map(|(mut pts, &delay)| {
                    if delay != 0.0 {
                        for p in pts.iter_mut() {
                            p[0] += delay;
                        }
                    }
                    pts
                })
                .collect();
            let _ = tx.send(CacheDecodeResult {
                channels: req_chs,
                points,
                vis_x_min: req_vis_min,
                vis_x_max: req_vis_max,
                delays,
            });
        });

        ctx.request_repaint();
    }

    /// Consume any finished background line-cache decode.
    /// Called once per frame (non-blocking).
    pub(crate) fn finish_cache_async(&mut self, ctx: &egui::Context) {
        let Some(rx) = self.cache_rx.take() else { return };
        let result = match rx.try_recv() {
            Ok(r) => r,
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                self.cache_rx = Some(rx);
                ctx.request_repaint();
                return;
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.cache_inflight_bounds = None;
                return;
            }
        };
        self.cache_inflight_bounds = None;

        for ((&ch_idx, pts), &delay) in result
            .channels
            .iter()
            .zip(result.points.into_iter())
            .zip(result.delays.iter())
        {
            if ch_idx >= self.cache.len() {
                continue;
            }
            self.cache[ch_idx] = Some(StripCache {
                points: pts,
                vis_x_min: result.vis_x_min,
                vis_x_max: result.vis_x_max,
                delay,
                ch_idx,
                is_coarse: false,
            });
        }

        // The first background decode lands after the initial frame already
        // consumed `needs_initial_fit` (with an empty cache). Re-arm it once
        // so Y auto-fits to the now-available data.
        if !self.y_auto_fitted {
            self.needs_initial_fit = true;
            self.y_auto_fitted = true;
        }
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

        let span = (vis_x_max - vis_x_min).max(1e-30);
        let tol = span * 1e-6;
        let needs_compute = match &self.measurement_cache[ch_idx] {
            Some((bounds, _)) => {
                (bounds.min()[0] - vis_x_min).abs() > tol
                    || (bounds.max()[0] - vis_x_max).abs() > tol
            }
            None => true,
        };

        if needs_compute {
            let data = self.data.as_mut().unwrap();

            // Prefer reusing the line-cache points (already decoded by the
            // background thread) for time-domain measurements — no extra TSZ
            // decode. Only valid when the cache covers the same range.
            let cache_matches = self
                .cache
                .get(ch_idx)
                .and_then(|c| c.as_ref())
                .map(|c| {
                    (c.vis_x_min - vis_x_min).abs() < tol
                        && (c.vis_x_max - vis_x_max).abs() < tol
                })
                .unwrap_or(false);

            let m = if cache_matches {
                let pts = self.cache[ch_idx].as_ref().unwrap().points.clone();
                Measurements::compute_from_cached(data, ch_idx, vis_x_min, vis_x_max, &pts)
            } else {
                // Cache not ready / range differs (e.g. cursor gate): fall
                // back to precomputed stats only — cheap, no decode.
                Measurements::compute_stats_only(data, ch_idx, vis_x_min, vis_x_max)
            };

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
        if let Ok(value) = input.parse::<f64>() {
            let target_t = self.goto_time_unit.to_seconds(value);
            let x_span = self.last_bounds.max()[0] - self.last_bounds.min()[0];
            let half = x_span / 2.0;
            self.push_zoom_history();
            self.last_bounds = PlotBounds::from_min_max(
                [target_t - half, self.last_bounds.min()[1]],
                [target_t + half, self.last_bounds.max()[1]],
            );
            self.needs_undo_zoom = true;
            self.status_message = format!(
                "Jumped to t = {:.6} {}",
                value,
                self.goto_time_unit.suffix(),
            );
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

        // Consume any finished background line-cache decode (non-blocking).
        self.finish_cache_async(ctx);

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

        if self.data.is_some() {
            let line_chs: Vec<usize> = (0..self.strips.len())
                .flat_map(|s_idx| self.strips[s_idx].channel_indices.iter().copied())
                .collect();
            if !line_chs.is_empty() {
                self.ensure_cache_async(ctx, &line_chs);
            }
        }

        // First-decode overlay: show spinner while initial decode runs so the
        // user doesn't see empty plots during the brief decode window.
        if self.data.is_some()
            && self.cache_inflight_bounds.is_some()
            && self.cache.iter().all(|c| c.is_none())
        {
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space(ui.available_height() / 3.0);
                    ui.spinner();
                    ui.add_space(8.0);
                    ui.label(
                        egui::RichText::new("Decoding waveform data…").size(18.0),
                    );
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
