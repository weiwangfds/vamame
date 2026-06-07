//! Main application state and UI rendering.
//!
//! CSV-only static waveform viewer backed by Polars. Each data channel occupies
//! a vertical strip with independently adjustable height. Drag a channel label
//! onto another strip to merge them. All strips share a linked x-axis for
//! synchronised zoom (scroll) and pan (drag). Zoom-aware min/max downsampling
//! keeps interactions fast even with 100 M+ rows.

use egui::{Color32, CursorIcon, Frame, Id, RichText, Sense, Vec2b};
use egui_plot::{
    CoordinatesFormatter, Corner, Legend, Line, Plot, PlotBounds, PlotPoints,
};

use crate::cursor::{CursorId, CursorMode, CursorState};
use crate::data::WaveformData;
use crate::export;
use crate::fft_analysis;
use crate::math_channel::{MathChannelDef, MathOp};
use crate::measurement::Measurements;

// ---------- constants ----------

const CHANNEL_COLORS: [Color32; 8] = [
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
const MAX_DISPLAY_POINTS: usize = 4000;

/// Minimum strip plot height in pixels.
const MIN_STRIP_HEIGHT: f32 = 60.0;

/// Screen-pixel proximity threshold for cursor drag detection.
const CURSOR_HIT_PX: f32 = 6.0;

// ---------- data model ----------

#[derive(Clone)]
pub struct ChannelState {
    pub name: String,
    pub visible: bool,
    pub delay: f64,
    pub color: Color32,
}

#[derive(Clone)]
pub struct Strip {
    pub channel_indices: Vec<usize>,
    pub height: f32,
}

#[derive(Clone, Copy)]
struct DragPayload {
    channel_idx: usize,
    source_strip: usize,
}

/// Cached downsampled points for one channel, keyed by visible x-range.
#[derive(Clone)]
struct StripCache {
    points: Vec<[f64; 2]>,
    vis_x_min: f64,
    vis_x_max: f64,
    delay: f64,
    ch_idx: usize,
}

impl StripCache {
    fn is_valid(&self, vis_x_min: f64, vis_x_max: f64, delay: f64, ch_idx: usize) -> bool {
        self.ch_idx == ch_idx
            && self.delay == delay
            && (self.vis_x_min - vis_x_min).abs() < f64::EPSILON
            && (self.vis_x_max - vis_x_max).abs() < f64::EPSILON
    }
}

// ---------- app ----------

pub struct OscilloscopeApp {
    channels: Vec<ChannelState>,
    strips: Vec<Strip>,

    /// Polars-backed waveform data.
    data: Option<WaveformData>,

    /// Per-channel downsample cache.
    cache: Vec<Option<StripCache>>,

    /// Last-known visible x-range, shared across all strips.
    last_bounds: PlotBounds,

    /// True until the first render after loading data.
    needs_initial_fit: bool,

    /// True when undo-zoom or goto-time needs manual bounds restoration.
    needs_undo_zoom: bool,

    /// Channel currently being renamed (inline edit), if any.
    editing_channel: Option<usize>,

    /// File path display.
    loaded_path: String,

    status_message: String,

    // --- Navigation ---
    /// Zoom history stack for undo.
    zoom_history: Vec<PlotBounds>,
    /// User input for "Go to time".
    goto_time_input: String,

    // --- Measurement ---
    /// Which channel to show in the measurement panel.
    measurement_channel: usize,
    /// Per-channel cached measurements + the bounds they were computed for.
    measurement_cache: Vec<Option<(PlotBounds, Measurements)>>,
    /// Toggle visibility of the measurement side-panel.
    show_measurement_panel: bool,

    // --- Cursor ---
    cursor: CursorState,

    // --- Export ---
    pending_screenshot_path: Option<String>,

    // --- FFT ---
    show_fft_window: bool,
    fft_channel: usize,
    fft_window_type: fft_analysis::WindowType,
    fft_scale: fft_analysis::FftScale,
    fft_cache: Option<(PlotBounds, usize, fft_analysis::WindowType, fft_analysis::FftScale, Vec<[f64; 2]>)>,

    // --- Math channels ---
    math_channels: Vec<MathChannelDef>,

    // --- XY mode ---
    show_xy_window: bool,
    xy_ch_x: usize,
    xy_ch_y: usize,

    // --- Math dialog ---
    show_math_dialog: bool,
    math_new_op: MathOp,
    math_new_src_a: usize,
    math_new_src_b: usize,
    pending_math_remove: Option<usize>,
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

            show_math_dialog: false,
            math_new_op: MathOp::Add,
            math_new_src_a: 0,
            math_new_src_b: 1,
            pending_math_remove: None,
        }
    }
}

// ---------- data loading ----------

impl OscilloscopeApp {
    fn load_csv_from_path(&mut self, path: &str) {
        match WaveformData::load_csv(path) {
            Ok(wd) => {
                let n_data = wd.n_channels();
                self.loaded_path = path.to_owned();
                self.data = Some(wd);

                self.channels = (0..n_data)
                    .map(|i| ChannelState {
                        name: format!("CH{}", i + 1),
                        visible: true,
                        delay: 0.0,
                        color: CHANNEL_COLORS[i % CHANNEL_COLORS.len()],
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
                self.last_bounds = PlotBounds::from_min_max(
                    [wd.x_min, -1.0],
                    [wd.x_max, 1.0],
                );

                self.cursor = CursorState::default();

                self.status_message = format!(
                    "Loaded {} channels × {} rows — scroll to zoom, drag to pan",
                    n_data, wd.n_rows,
                );
            }
            Err(e) => {
                self.status_message = format!("Error: {}", e);
            }
        }
    }

    /// Ensure the downsample cache for one channel is up-to-date.
    fn ensure_cache(&mut self, ch_idx: usize) {
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
    fn ensure_measurements(&mut self, ch_idx: usize) {
        let Some(ref data) = self.data else { return };
        if ch_idx >= self.channels.len() || ch_idx >= self.measurement_cache.len() {
            return;
        }

        let vis_x_min = self.last_bounds.min()[0];
        let vis_x_max = self.last_bounds.max()[0];

        let needs_compute = match &self.measurement_cache[ch_idx] {
            Some((bounds, _)) => {
                (bounds.min()[0] - vis_x_min).abs() > f64::EPSILON
                    || (bounds.max()[0] - vis_x_max).abs() > f64::EPSILON
            }
            None => true,
        };

        if needs_compute {
            let m = Measurements::compute(data, ch_idx, vis_x_min, vis_x_max);
            self.measurement_cache[ch_idx] = Some((self.last_bounds, m));
        }
    }

    // --- Navigation helpers ---

    /// Push current bounds to zoom history (call before changing bounds).
    fn push_zoom_history(&mut self) {
        if self.last_bounds != PlotBounds::NOTHING {
            self.zoom_history.push(self.last_bounds);
            // Keep a reasonable max
            if self.zoom_history.len() > 50 {
                self.zoom_history.remove(0);
            }
        }
    }

    /// Restore previous zoom level from history.
    fn undo_zoom(&mut self) {
        if let Some(prev) = self.zoom_history.pop() {
            self.last_bounds = prev;
            // The central panel will use these bounds on next frame.
            // To force it, we set needs_initial_fit = false but
            // store the target bounds for manual application.
            self.needs_undo_zoom = true;
            self.status_message = "Zoom restored".to_owned();
        }
    }

    /// Jump to a specific time value entered by the user.
    fn goto_time(&mut self) {
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
            self.status_message = format!("Jumped to t = {}", Measurements::format_value(target_t, "s"));
        } else {
            self.status_message = "Invalid time value".to_owned();
        }
    }
}

// ---------- eframe::App ----------

impl eframe::App for OscilloscopeApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.check_screenshot_events(ctx);
        self.draw_toolbar(ctx);
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
        self.draw_math_dialog(ctx);
    }
}

// ---------- toolbar ----------

impl OscilloscopeApp {
    fn draw_toolbar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("Oscilloscope");
                ui.separator();

                if ui.button("Open CSV...").clicked() {
                    if let Some(path) = rfd::FileDialog::new()
                        .add_filter("CSV", &["csv", "txt"])
                        .add_filter("All", &["*"])
                        .pick_file()
                    {
                        self.load_csv_from_path(&path.display().to_string());
                    }
                }

                if self.data.is_some() {
                    // Fit button
                    if ui.button("Fit").clicked() {
                        self.push_zoom_history();
                        self.needs_initial_fit = true;
                    }

                    // Undo zoom
                    if ui.button("Undo Zoom").clicked() {
                        self.undo_zoom();
                    }

                    // Go to time
                    ui.label("Go t:");
                    let goto_response = ui.add(
                        egui::TextEdit::singleline(&mut self.goto_time_input)
                            .desired_width(80.0)
                            .hint_text("e.g. 1e-3"),
                    );
                    if goto_response.lost_focus()
                        && ui.input(|i| i.key_pressed(egui::Key::Enter))
                    {
                        self.goto_time();
                    }

                    ui.separator();

                    // Cursor mode
                    let cursor_label = match self.cursor.mode {
                        CursorMode::Off => "Cursor: Off",
                        CursorMode::Vertical => "Cursor: Vert (dT)",
                        CursorMode::Horizontal => "Cursor: Horiz (dV)",
                    };
                    ui.menu_button(cursor_label, |ui| {
                        let x_min = self.last_bounds.min()[0];
                        let x_max = self.last_bounds.max()[0];
                        let y_min = self.last_bounds.min()[1];
                        let y_max = self.last_bounds.max()[1];

                        if ui
                            .selectable_label(self.cursor.mode == CursorMode::Off, "Off")
                            .clicked()
                        {
                            self.cursor.mode = CursorMode::Off;
                            self.cursor.dragging = None;
                            ui.close_menu();
                        }
                        if ui
                            .selectable_label(
                                self.cursor.mode == CursorMode::Vertical,
                                "Vertical  (Delta-T)",
                            )
                            .clicked()
                        {
                            self.cursor.set_mode(CursorMode::Vertical, x_min, x_max);
                            ui.close_menu();
                        }
                        if ui
                            .selectable_label(
                                self.cursor.mode == CursorMode::Horizontal,
                                "Horizontal  (Delta-V)",
                            )
                            .clicked()
                        {
                            self.cursor.set_mode(CursorMode::Horizontal, y_min, y_max);
                            ui.close_menu();
                        }
                    });

                    ui.separator();

                    // Export
                    ui.menu_button("Export", |ui| {
                        if ui.button("Export CSV...").clicked() {
                            self.do_export_csv();
                            ui.close_menu();
                        }
                        if ui.button("Export PNG...").clicked() {
                            self.do_export_png(ctx);
                            ui.close_menu();
                        }
                    });

                    ui.separator();

                    // Measurement panel toggle
                    ui.toggle_value(&mut self.show_measurement_panel, "Measurements");

                    ui.separator();

                    // FFT
                    if ui.button("FFT").clicked() {
                        self.show_fft_window = true;
                    }

                    // XY Mode
                    if ui.button("XY").clicked() {
                        self.show_xy_window = true;
                    }

                    // Math channel
                    if ui.button("Math...").clicked() {
                        self.show_math_dialog = true;
                    }
                }

                if !self.loaded_path.is_empty() {
                    ui.separator();
                    let name = std::path::Path::new(&self.loaded_path)
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| self.loaded_path.clone());
                    ui.label(RichText::new(name).small().color(Color32::GRAY));
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        RichText::new(&self.status_message)
                            .color(Color32::GRAY)
                            .small(),
                    );
                });
            });
        });
    }
}

// ---------- measurement panel ----------

impl OscilloscopeApp {
    fn draw_measurement_panel(&mut self, ctx: &egui::Context) {
        egui::SidePanel::right("measurement_panel")
            .min_width(220.0)
            .default_width(270.0)
            .show(ctx, |ui| {
                ui.heading("Measurements");
                ui.add_space(4.0);

                // Channel selector
                ui.horizontal(|ui| {
                    ui.label("Channel:");
                    let ch = &mut self.measurement_channel;
                    let ch_count = self.channels.len();
                    egui::ComboBox::from_id_salt("meas_ch_select")
                        .selected_text(
                            self.channels
                                .get(*ch)
                                .map(|c| c.name.as_str())
                                .unwrap_or("---"),
                        )
                        .show_ui(ui, |ui| {
                            for i in 0..ch_count {
                                let label = &self.channels[i].name;
                                ui.selectable_value(ch, i, label);
                            }
                        });
                });

                ui.separator();
                ui.add_space(2.0);

                // Compute / retrieve measurements
                let ch_idx = self.measurement_channel;
                self.ensure_measurements(ch_idx);

                let measurements = self
                    .measurement_cache
                    .get(ch_idx)
                    .and_then(|opt| opt.as_ref())
                    .map(|(_, m)| m.clone());

                if let Some(m) = measurements {
                    // Voltage section
                    ui.label(RichText::new("Voltage").strong());
                    meas_row(ui, "Vpp", m.vpp, "V");
                    meas_row(ui, "Vmax", m.vmax, "V");
                    meas_row(ui, "Vmin", m.vmin, "V");
                    meas_row(ui, "Vmean", m.vmean, "V");
                    meas_row(ui, "Vrms", m.vrms, "V");

                    ui.add_space(4.0);

                    // Time section
                    ui.label(RichText::new("Time").strong());
                    if let Some(freq) = m.frequency {
                        meas_row(ui, "Freq", freq, "Hz");
                    } else {
                        meas_row_na(ui, "Freq");
                    }
                    if let Some(period) = m.period {
                        meas_row(ui, "Period", period, "s");
                    } else {
                        meas_row_na(ui, "Period");
                    }
                    if let Some(rt) = m.rise_time {
                        meas_row(ui, "Rise", rt, "s");
                    } else {
                        meas_row_na(ui, "Rise");
                    }
                    if let Some(ft) = m.fall_time {
                        meas_row(ui, "Fall", ft, "s");
                    } else {
                        meas_row_na(ui, "Fall");
                    }
                    if let Some(dc) = m.duty_cycle {
                        meas_row(ui, "Duty", dc, "%");
                    } else {
                        meas_row_na(ui, "Duty");
                    }
                    if let Some(pw) = m.pos_width {
                        meas_row(ui, "+Width", pw, "s");
                    } else {
                        meas_row_na(ui, "+Width");
                    }
                    if let Some(nw) = m.neg_width {
                        meas_row(ui, "-Width", nw, "s");
                    } else {
                        meas_row_na(ui, "-Width");
                    }

                    // Cursor section
                    if self.cursor.mode != CursorMode::Off {
                        ui.add_space(6.0);
                        ui.separator();
                        ui.add_space(2.0);
                        ui.label(RichText::new("Cursor").strong());
                        let delta = self.cursor.delta();
                        match self.cursor.mode {
                            CursorMode::Vertical => {
                                meas_row(ui, "Delta-T", delta, "s");
                                if delta > 0.0 {
                                    meas_row(ui, "1/Delta-T", 1.0 / delta, "Hz");
                                }
                                meas_row(
                                    ui,
                                    "T-A",
                                    self.cursor.pos_a,
                                    "s",
                                );
                                meas_row(
                                    ui,
                                    "T-B",
                                    self.cursor.pos_b,
                                    "s",
                                );
                            }
                            CursorMode::Horizontal => {
                                meas_row(ui, "Delta-V", delta, "V");
                                meas_row(
                                    ui,
                                    "V-A",
                                    self.cursor.pos_a,
                                    "V",
                                );
                                meas_row(
                                    ui,
                                    "V-B",
                                    self.cursor.pos_b,
                                    "V",
                                );
                            }
                            CursorMode::Off => {}
                        }
                    }
                } else {
                    ui.label(
                        RichText::new("No measurement data")
                            .small()
                            .color(Color32::GRAY),
                    );
                }
            });
    }
}

fn meas_row(ui: &mut egui::Ui, name: &str, value: f64, unit: &str) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(name).small());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(RichText::new(Measurements::format_value(value, unit)).small().monospace());
        });
    });
}

fn meas_row_na(ui: &mut egui::Ui, name: &str) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(name).small());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(RichText::new("---").small().color(Color32::GRAY).monospace());
        });
    });
}

// ---------- export actions ----------

impl OscilloscopeApp {
    fn do_export_csv(&mut self) {
        let Some(ref data) = self.data else { return };

        if let Some(path) = rfd::FileDialog::new()
            .add_filter("CSV", &["csv"])
            .set_file_name("export.csv")
            .save_file()
        {
            let vis_x_min = self.last_bounds.min()[0];
            let vis_x_max = self.last_bounds.max()[0];
            let ch_indices: Vec<usize> = (0..data.n_channels()).collect();
            let path_str = path.display().to_string();
            match export::export_csv(data, &ch_indices, vis_x_min, vis_x_max, &path_str) {
                Ok(()) => {
                    self.status_message = format!("CSV exported to {}", path_str);
                }
                Err(e) => {
                    self.status_message = format!("Export error: {}", e);
                }
            }
        }
    }

    fn do_export_png(&mut self, ctx: &egui::Context) {
        // Request screenshot; will be received as an Event in a later frame.
        // The user picks a path first, then we save when the image arrives.
        if let Some(path) = rfd::FileDialog::new()
            .add_filter("PNG", &["png"])
            .set_file_name("screenshot.png")
            .save_file()
        {
            self.pending_screenshot_path = Some(path.display().to_string());
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(
                egui::UserData::new("export_png".to_owned()),
            ));
            self.status_message = "Capturing screenshot...".to_owned();
        }
    }

    /// Check for pending screenshot events and save them.
    fn check_screenshot_events(&mut self, ctx: &egui::Context) {
        if self.pending_screenshot_path.is_none() {
            return;
        }
        let mut found = false;
        ctx.input(|i| {
            for event in i.events.iter() {
                if let egui::Event::Screenshot { image, .. } = event {
                    found = true;
                    if let Some(path) = self.pending_screenshot_path.take() {
                        match export::save_png(image, &path) {
                            Ok(()) => {
                                self.status_message = format!("PNG saved to {}", path);
                            }
                            Err(e) => {
                                self.status_message = format!("PNG error: {}", e);
                            }
                        }
                    }
                }
            }
        });
        if !found {
            // Screenshot hasn't arrived yet — keep waiting.
            let _ = &self.pending_screenshot_path;
        }
    }
}

// ---------- central area ----------

impl OscilloscopeApp {
    fn draw_central(&mut self, ctx: &egui::Context) {
        if self.data.is_none() {
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space(ui.available_height() / 3.0);
                    ui.heading("No data loaded");
                    ui.add_space(10.0);
                    ui.label("Click \"Open CSV...\" in the toolbar to import a waveform file.");
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new(
                            "Expected format: comma-separated numeric columns, \
                              one sample per row. Column 0 is treated as the time axis.",
                        )
                        .small()
                        .color(Color32::GRAY),
                    );
                });
            });
            return;
        }

        let link_id = Id::new("osc_x_link");
        let cursor_link_id = Id::new("osc_cursor_link");
        let time_span = self.data.as_ref().map(|d| d.time_span).unwrap_or(1.0);

        egui::CentralPanel::default().show(ctx, |ui| {
            // On first display, distribute heights to fill available space.
            let available = ui.available_height();
            let total: f32 = self.strips.iter().map(|s| s.height).sum();
            if total.lt(&available) && !self.strips.is_empty() {
                let per = (available / self.strips.len() as f32).max(MIN_STRIP_HEIGHT);
                for s in &mut self.strips {
                    s.height = per;
                }
            }

            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    for s_idx in 0..self.strips.len() {
                        let mut split_requested = false;
                        let strip_height = self.strips[s_idx].height;

                        // ======== header ========
                        let strip_chs = self.strips[s_idx].channel_indices.clone();
                        let mut channel_to_add: Option<usize> = None;
                        let mut channel_to_remove: Option<usize> = None;

                        ui.horizontal_wrapped(|ui| {
                            ui.label(
                                RichText::new(format!("Strip {}", s_idx + 1))
                                    .small()
                                    .strong(),
                            );
                            ui.separator();

                            for &ch_idx in &strip_chs {
                                if ch_idx >= self.channels.len() {
                                    continue;
                                }
                                let ch_color = self.channels[ch_idx].color;

                                // -- DnD drag handle (small colored square) --
                                let drag_id = ui.id().with("ch_drag").with(ch_idx);
                                ui.dnd_drag_source(
                                    drag_id,
                                    DragPayload {
                                        channel_idx: ch_idx,
                                        source_strip: s_idx,
                                    },
                                    |ui| {
                                        let (rect, _) = ui.allocate_exact_size(
                                            egui::vec2(12.0, 12.0),
                                            Sense::hover(),
                                        );
                                        ui.painter().rect_filled(rect, 2.0, ch_color);
                                    },
                                );

                                // -- channel name (double-click to rename) --
                                if self.editing_channel == Some(ch_idx) {
                                    let name = &mut self.channels[ch_idx].name;
                                    let edit_id = ui.id().with("ch_edit").with(ch_idx);
                                    let response = ui.add(
                                        egui::TextEdit::singleline(name)
                                            .id(edit_id)
                                            .desired_width(80.0)
                                            .text_color(ch_color),
                                    );
                                    if response.lost_focus() {
                                        self.editing_channel = None;
                                    }
                                } else {
                                    let label_response = ui.colored_label(
                                        ch_color,
                                        &self.channels[ch_idx].name,
                                    );
                                    if label_response.double_clicked() {
                                        self.editing_channel = Some(ch_idx);
                                    }
                                }

                                // -- delay --
                                ui.label(
                                    RichText::new("d:").small().color(Color32::GRAY),
                                );
                                let max_delay = time_span * 0.5;
                                ui.add(
                                    egui::DragValue::new(&mut self.channels[ch_idx].delay)
                                        .range(-max_delay..=max_delay)
                                        .speed(time_span * 0.001)
                                        .fixed_decimals(2)
                                        .suffix("s"),
                                );

                                // -- remove from strip --
                                if ui
                                    .button(RichText::new("×").small())
                                    .on_hover_text("Remove from strip")
                                    .clicked()
                                {
                                    channel_to_remove = Some(ch_idx);
                                }

                                ui.separator();
                            }

                            // -- "+" overlay button --
                            let available_chs: Vec<(usize, String, Color32)> = (0
                                ..self.channels.len())
                                .filter(|i| !strip_chs.contains(i))
                                .map(|i| {
                                    (
                                        i,
                                        self.channels[i].name.clone(),
                                        self.channels[i].color,
                                    )
                                })
                                .collect();

                            if !available_chs.is_empty() {
                                ui.menu_button(RichText::new("+ Overlay").small(), |ui| {
                                    for (ch_idx, name, color) in &available_chs {
                                        if ui
                                            .button(
                                                RichText::new(format!("  {}  +", name))
                                                    .color(*color),
                                            )
                                            .clicked()
                                        {
                                            channel_to_add = Some(*ch_idx);
                                        }
                                    }
                                });
                            }

                            // -- split --
                            if strip_chs.len() > 1 {
                                if ui.button("Split").clicked() {
                                    split_requested = true;
                                }
                            }
                        });

                        // ======== ensure cache ========
                        let strip_chs = self.strips[s_idx].channel_indices.clone();
                        for &ch_idx in &strip_chs {
                            self.ensure_cache(ch_idx);
                        }

                        // ======== plot ========
                        let frame = Frame::default()
                            .stroke(egui::Stroke::new(
                                1.0,
                                Color32::from_rgba_unmultiplied(255, 255, 255, 30),
                            ))
                            .inner_margin(2.0);

                        let initial_fit = self.needs_initial_fit;
                        let undo_zoom = self.needs_undo_zoom;
                        let undo_bounds = self.last_bounds;

                        // Snapshot cursor state for the closure.
                        let cursor_mode = self.cursor.mode;
                        let cursor_a = self.cursor.pos_a;
                        let cursor_b = self.cursor.pos_b;

                        let (_, dropped) = ui.dnd_drop_zone::<DragPayload, _>(frame, |ui| {
                            let plot_id = ui.id().with("strip_plot").with(s_idx);
                            let show_x_axis = s_idx == self.strips.len() - 1;

                            let plot = Plot::new(plot_id)
                                .legend(Legend::default().position(Corner::RightTop))
                                .show_axes(Vec2b::new(show_x_axis, true))
                                .show_grid([true, true])
                                .link_axis(link_id, Vec2b::new(true, false))
                                .link_cursor(cursor_link_id, Vec2b::new(true, false))
                                .allow_zoom(Vec2b::new(true, false))
                                .allow_scroll(Vec2b::new(true, false))
                                .allow_drag(Vec2b::new(true, false))
                                .y_axis_min_width(80.0)
                                .coordinates_formatter(
                                    Corner::LeftBottom,
                                    CoordinatesFormatter::new(|pt, bounds| {
                                        let x_span = bounds.max()[0] - bounds.min()[0];
                                        let y_span = bounds.max()[1] - bounds.min()[1];
                                        let x_div = Measurements::format_value(x_span / 10.0, "s/div");
                                        let y_div = Measurements::format_value(y_span / 8.0, "V/div");
                                        format!(
                                            "t = {:.3e} s  V = {:.6} V\n{}  {}",
                                            pt.x, pt.y, x_div, y_div,
                                        )
                                    }),
                                )
                                .x_axis_label(if show_x_axis { "Time (s)" } else { "" })
                                .y_axis_label("V")
                                .height(strip_height);

                            let plot_response = plot.show(ui, |plot_ui| {
                                if initial_fit {
                                    plot_ui.set_auto_bounds(Vec2b::new(true, true));
                                } else if undo_zoom {
                                    // Restore exact bounds from undo/goto
                                    plot_ui.set_plot_bounds(undo_bounds);
                                } else {
                                    plot_ui.set_auto_bounds(Vec2b::new(false, true));
                                }

                                for &ch_idx in &strip_chs {
                                    if ch_idx >= self.channels.len()
                                        || !self.channels[ch_idx].visible
                                    {
                                        continue;
                                    }
                                    if let Some(ref cached) = self.cache[ch_idx] {
                                        let ch = &self.channels[ch_idx];
                                        let line =
                                            Line::new(PlotPoints::from(cached.points.clone()))
                                                .color(ch.color)
                                                .width(1.5)
                                                .name(&ch.name);
                                        plot_ui.line(line);
                                    }
                                }

                                // --- Draw cursor lines ---
                                if cursor_mode != CursorMode::Off {
                                    draw_cursor_lines(plot_ui, cursor_mode, cursor_a, cursor_b);
                                }
                            });

                            // --- Handle cursor drag interaction ---
                            if cursor_mode != CursorMode::Off {
                                self.handle_cursor_interaction(&plot_response, s_idx);
                            }

                            let bounds = plot_response.transform.bounds();
                            self.last_bounds = *bounds;
                        });

                        if self.needs_initial_fit && s_idx == 0 {
                            self.needs_initial_fit = false;
                        }
                        if self.needs_undo_zoom && s_idx == 0 {
                            self.needs_undo_zoom = false;
                        }

                        // -- resize handle --
                        if s_idx < self.strips.len() - 1 {
                            self.draw_resize_handle(ui, s_idx);
                        } else {
                            self.draw_last_resize_handle(ui, s_idx);
                        }

                        // ======== deferred strip mutations ========

                        // DnD drop
                        if let Some(payload) = dropped {
                            if payload.source_strip != s_idx {
                                self.move_channel_to_strip(payload.channel_idx, s_idx);
                                break;
                            }
                        }

                        // Overlay add
                        if let Some(ch_idx) = channel_to_add {
                            if !self.strips[s_idx].channel_indices.contains(&ch_idx) {
                                self.strips[s_idx].channel_indices.push(ch_idx);
                                for (i, strip) in self.strips.iter_mut().enumerate() {
                                    if i != s_idx {
                                        strip.channel_indices.retain(|&c| c != ch_idx);
                                    }
                                }
                                self.strips.retain(|s| !s.channel_indices.is_empty());
                            }
                            break;
                        }

                        // Remove
                        if let Some(ch_idx) = channel_to_remove {
                            self.strips[s_idx].channel_indices.retain(|&i| i != ch_idx);
                            let in_any = self
                                .strips
                                .iter()
                                .any(|s| s.channel_indices.contains(&ch_idx));
                            if !in_any {
                                self.strips.insert(
                                    s_idx + 1,
                                    Strip {
                                        channel_indices: vec![ch_idx],
                                        height: strip_height,
                                    },
                                );
                            }
                            self.strips.retain(|s| !s.channel_indices.is_empty());
                            break;
                        }

                        // Split
                        if split_requested {
                            self.split_strip(s_idx);
                            break;
                        }
                    }
                });
        });
    }

    // ---- cursor rendering ----

    fn handle_cursor_interaction(
        &mut self,
        plot_response: &egui_plot::PlotResponse<()>,
        _s_idx: usize,
    ) {
        let mode = self.cursor.mode;
        let response = &plot_response.response;
        let transform = &plot_response.transform;

        // Convert cursor positions to screen coordinates (only the relevant axis).
        let (screen_a, screen_b) = match mode {
            CursorMode::Vertical => {
                let sa = transform.position_from_point(&egui_plot::PlotPoint::new(
                    self.cursor.pos_a,
                    0.0,
                ));
                let sb = transform.position_from_point(&egui_plot::PlotPoint::new(
                    self.cursor.pos_b,
                    0.0,
                ));
                (sa.x, sb.x)
            }
            CursorMode::Horizontal => {
                let sa = transform.position_from_point(&egui_plot::PlotPoint::new(
                    0.0,
                    self.cursor.pos_a,
                ));
                let sb = transform.position_from_point(&egui_plot::PlotPoint::new(
                    0.0,
                    self.cursor.pos_b,
                ));
                (sa.y, sb.y)
            }
            CursorMode::Off => return,
        };

        // Check for mouse interaction.
        if response.dragged() {
            if let Some(dragging) = self.cursor.dragging {
                // Update the dragged cursor position from the mouse position.
                if let Some(mouse_pos) = response.interact_pointer_pos() {
                    let plot_pt = transform.value_from_position(mouse_pos);
                    match mode {
                        CursorMode::Vertical => {
                            match dragging {
                                CursorId::A => self.cursor.pos_a = plot_pt.x,
                                CursorId::B => self.cursor.pos_b = plot_pt.x,
                            }
                        }
                        CursorMode::Horizontal => {
                            match dragging {
                                CursorId::A => self.cursor.pos_a = plot_pt.y,
                                CursorId::B => self.cursor.pos_b = plot_pt.y,
                            }
                        }
                        CursorMode::Off => {}
                    }
                }
            } else {
                // Start dragging: check if mouse is near a cursor.
                if let Some(mouse_pos) = response.hover_pos() {
                    let mouse_coord = match mode {
                        CursorMode::Vertical => mouse_pos.x,
                        CursorMode::Horizontal => mouse_pos.y,
                        CursorMode::Off => return,
                    };
                    let dist_a = (mouse_coord - screen_a).abs();
                    let dist_b = (mouse_coord - screen_b).abs();
                    if dist_a < CURSOR_HIT_PX && dist_a <= dist_b {
                        self.cursor.dragging = Some(CursorId::A);
                    } else if dist_b < CURSOR_HIT_PX {
                        self.cursor.dragging = Some(CursorId::B);
                    }
                }
            }
        } else {
            self.cursor.dragging = None;
        }

        // Show resize cursor when hovering near a cursor line.
        if let Some(mouse_pos) = response.hover_pos() {
            let mouse_coord = match mode {
                CursorMode::Vertical => mouse_pos.x,
                CursorMode::Horizontal => mouse_pos.y,
                CursorMode::Off => return,
            };
            if (mouse_coord - screen_a).abs() < CURSOR_HIT_PX
                || (mouse_coord - screen_b).abs() < CURSOR_HIT_PX
            {
                response.clone().on_hover_cursor(CursorIcon::ResizeHorizontal);
            }
        }
    }

    fn draw_resize_handle(&mut self, ui: &mut egui::Ui, above: usize) {
        let handle_height = 6.0;
        let below = above + 1;

        let (rect, response) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), handle_height),
            Sense::drag(),
        );

        let cy = rect.center().y;
        ui.painter().line_segment(
            [
                egui::pos2(rect.left() + 4.0, cy),
                egui::pos2(rect.right() - 4.0, cy),
            ],
            egui::Stroke::new(2.0, Color32::from_rgba_unmultiplied(255, 255, 255, 60)),
        );

        if response.hovered() || response.dragged() {
            ui.ctx().set_cursor_icon(CursorIcon::ResizeVertical);
        }

        if response.dragged() {
            let dy = response.drag_delta().y;
            let h_above = self.strips[above].height;
            let h_below = self.strips[below].height;
            let new_above = (h_above + dy).max(MIN_STRIP_HEIGHT);
            let actual_dy = new_above - h_above;
            let new_below = (h_below - actual_dy).max(MIN_STRIP_HEIGHT);
            self.strips[above].height = new_above;
            self.strips[below].height = new_below;
        }
    }

    fn draw_last_resize_handle(&mut self, ui: &mut egui::Ui, strip: usize) {
        let handle_height = 6.0;

        let (rect, response) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), handle_height),
            Sense::drag(),
        );

        let cy = rect.center().y;
        ui.painter().line_segment(
            [
                egui::pos2(rect.left() + 4.0, cy),
                egui::pos2(rect.right() - 4.0, cy),
            ],
            egui::Stroke::new(2.0, Color32::from_rgba_unmultiplied(255, 255, 255, 60)),
        );

        if response.hovered() || response.dragged() {
            ui.ctx().set_cursor_icon(CursorIcon::ResizeVertical);
        }

        if response.dragged() {
            let dy = response.drag_delta().y;
            let h = self.strips[strip].height;
            self.strips[strip].height = (h + dy).max(MIN_STRIP_HEIGHT);
        }
    }

    fn move_channel_to_strip(&mut self, ch_idx: usize, target_strip: usize) {
        for strip in &mut self.strips {
            strip.channel_indices.retain(|&i| i != ch_idx);
        }
        self.strips.retain(|s| !s.channel_indices.is_empty());
        let target = target_strip.min(self.strips.len().saturating_sub(1));
        if target < self.strips.len() {
            self.strips[target].channel_indices.push(ch_idx);
        } else {
            self.strips.push(Strip {
                channel_indices: vec![ch_idx],
                height: 150.0,
            });
        }
        self.status_message = format!(
            "Merged {} into strip {}",
            self.channels[ch_idx].name,
            target + 1,
        );
    }

    fn split_strip(&mut self, s_idx: usize) {
        if s_idx >= self.strips.len() {
            return;
        }
        let chs = self.strips[s_idx].channel_indices.clone();
        if chs.len() <= 1 {
            return;
        }
        let h = self.strips[s_idx].height;
        self.strips[s_idx].channel_indices = vec![chs[0]];
        for (off, &idx) in chs.iter().skip(1).enumerate() {
            self.strips.insert(
                s_idx + 1 + off,
                Strip {
                    channel_indices: vec![idx],
                    height: h,
                },
            );
        }
        self.status_message = format!("Split strip {} into {} strips", s_idx + 1, chs.len());
    }
}

// ---------- cursor line drawing ----------

fn draw_cursor_lines(
    plot_ui: &mut egui_plot::PlotUi,
    mode: CursorMode,
    pos_a: f64,
    pos_b: f64,
) {
    let bounds = plot_ui.plot_bounds();
    let color_a = Color32::from_rgba_unmultiplied(255, 255, 100, 180);
    let color_b = Color32::from_rgba_unmultiplied(100, 255, 255, 180);

    match mode {
        CursorMode::Vertical => {
            let y_min = bounds.min()[1] - 1e6; // extend well beyond view
            let y_max = bounds.max()[1] + 1e6;
            plot_ui.line(
                Line::new(PlotPoints::from(vec![
                    [pos_a, y_min],
                    [pos_a, y_max],
                ]))
                .color(color_a)
                .width(1.5),
            );
            plot_ui.line(
                Line::new(PlotPoints::from(vec![
                    [pos_b, y_min],
                    [pos_b, y_max],
                ]))
                .color(color_b)
                .width(1.5),
            );
        }
        CursorMode::Horizontal => {
            let x_min = bounds.min()[0] - 1e6;
            let x_max = bounds.max()[0] + 1e6;
            plot_ui.line(
                Line::new(PlotPoints::from(vec![
                    [x_min, pos_a],
                    [x_max, pos_a],
                ]))
                .color(color_a)
                .width(1.5),
            );
            plot_ui.line(
                Line::new(PlotPoints::from(vec![
                    [x_min, pos_b],
                    [x_max, pos_b],
                ]))
                .color(color_b)
                .width(1.5),
            );
        }
        CursorMode::Off => {}
    }
}

// ---------- FFT window ----------

impl OscilloscopeApp {
    fn draw_fft_window(&mut self, ctx: &egui::Context) {
        let Some(ref data) = self.data else { return };

        egui::Window::new("FFT Spectrum Analysis")
            .open(&mut self.show_fft_window)
            .default_size([700.0, 400.0])
            .min_size([400.0, 250.0])
            .show(ctx, |ui| {
                // Controls
                ui.horizontal(|ui| {
                    ui.label("Channel:");
                    let n_ch = data.n_channels();
                    egui::ComboBox::from_id_salt("fft_ch_select")
                        .selected_text(
                            self.channels
                                .get(self.fft_channel)
                                .map(|c| c.name.as_str())
                                .unwrap_or("---"),
                        )
                        .show_ui(ui, |ui| {
                            for i in 0..n_ch.min(self.channels.len()) {
                                ui.selectable_value(
                                    &mut self.fft_channel,
                                    i,
                                    &self.channels[i].name,
                                );
                            }
                        });

                    ui.separator();

                    ui.label("Window:");
                    egui::ComboBox::from_id_salt("fft_window_select")
                        .selected_text(self.fft_window_type.to_string())
                        .show_ui(ui, |ui| {
                            for wt in [
                                fft_analysis::WindowType::Rectangle,
                                fft_analysis::WindowType::Hanning,
                                fft_analysis::WindowType::BlackmanHarris,
                            ] {
                                ui.selectable_value(
                                    &mut self.fft_window_type,
                                    wt,
                                    wt.to_string(),
                                );
                            }
                        });

                    ui.separator();

                    ui.label("Scale:");
                    egui::ComboBox::from_id_salt("fft_scale_select")
                        .selected_text(match self.fft_scale {
                            fft_analysis::FftScale::Db => "dB",
                            fft_analysis::FftScale::Linear => "Linear",
                        })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut self.fft_scale,
                                fft_analysis::FftScale::Db,
                                "dB",
                            );
                            ui.selectable_value(
                                &mut self.fft_scale,
                                fft_analysis::FftScale::Linear,
                                "Linear",
                            );
                        });
                });

                ui.add_space(4.0);

                // Compute / retrieve cached FFT
                let vis_x_min = self.last_bounds.min()[0];
                let vis_x_max = self.last_bounds.max()[0];
                let needs_recompute = match &self.fft_cache {
                    Some((bounds, ch, wt, sc, _)) => {
                        (bounds.min()[0] - vis_x_min).abs() > f64::EPSILON
                            || (bounds.max()[0] - vis_x_max).abs() > f64::EPSILON
                            || *ch != self.fft_channel
                            || *wt != self.fft_window_type
                            || *sc != self.fft_scale
                    }
                    None => true,
                };

                if needs_recompute {
                    let points =
                        data.get_raw_points(self.fft_channel, vis_x_min, vis_x_max, 131_072);
                    let spectrum =
                        fft_analysis::compute_fft(&points, self.fft_window_type, self.fft_scale);
                    self.fft_cache = Some((
                        self.last_bounds,
                        self.fft_channel,
                        self.fft_window_type,
                        self.fft_scale,
                        spectrum,
                    ));
                }

                let spectrum = self
                    .fft_cache
                    .as_ref()
                    .map(|(_, _, _, _, s)| s.clone())
                    .unwrap_or_default();

                let y_label = match self.fft_scale {
                    fft_analysis::FftScale::Db => "Magnitude (dB)",
                    fft_analysis::FftScale::Linear => "Magnitude",
                };

                Plot::new("fft_plot")
                    .show_grid([true, true])
                    .x_axis_label("Frequency (Hz)")
                    .y_axis_label(y_label)
                    .allow_zoom(Vec2b::new(true, true))
                    .allow_drag(Vec2b::new(true, true))
                    .height(ui.available_height() - 10.0)
                    .show(ui, |plot_ui| {
                        if !spectrum.is_empty() {
                            let line = Line::new(PlotPoints::from(spectrum))
                                .color(Color32::from_rgb(0, 200, 255))
                                .width(1.5);
                            plot_ui.line(line);
                        }
                    });
            });
    }
}

// ---------- XY mode window ----------

impl OscilloscopeApp {
    fn draw_xy_window(&mut self, ctx: &egui::Context) {
        let Some(ref data) = self.data else { return };

        egui::Window::new("XY Mode (Lissajous)")
            .open(&mut self.show_xy_window)
            .default_size([500.0, 500.0])
            .min_size([300.0, 300.0])
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label("X:");
                    let n_ch = data.n_channels();
                    egui::ComboBox::from_id_salt("xy_ch_x")
                        .selected_text(
                            self.channels
                                .get(self.xy_ch_x)
                                .map(|c| c.name.as_str())
                                .unwrap_or("---"),
                        )
                        .show_ui(ui, |ui| {
                            for i in 0..n_ch.min(self.channels.len()) {
                                ui.selectable_value(
                                    &mut self.xy_ch_x,
                                    i,
                                    &self.channels[i].name,
                                );
                            }
                        });

                    ui.separator();

                    ui.label("Y:");
                    egui::ComboBox::from_id_salt("xy_ch_y")
                        .selected_text(
                            self.channels
                                .get(self.xy_ch_y)
                                .map(|c| c.name.as_str())
                                .unwrap_or("---"),
                        )
                        .show_ui(ui, |ui| {
                            for i in 0..n_ch.min(self.channels.len()) {
                                ui.selectable_value(
                                    &mut self.xy_ch_y,
                                    i,
                                    &self.channels[i].name,
                                );
                            }
                        });
                });

                ui.add_space(4.0);

                let vis_x_min = self.last_bounds.min()[0];
                let vis_x_max = self.last_bounds.max()[0];

                let x_pts = data.get_raw_points(self.xy_ch_x, vis_x_min, vis_x_max, MAX_DISPLAY_POINTS);
                let y_pts = data.get_raw_points(self.xy_ch_y, vis_x_min, vis_x_max, MAX_DISPLAY_POINTS);

                // Align by minimum length
                let n = x_pts.len().min(y_pts.len());
                let xy_points: Vec<[f64; 2]> = (0..n)
                    .map(|i| [x_pts[i][1], y_pts[i][1]])
                    .collect();

                let x_name = self.channels.get(self.xy_ch_x).map(|c| c.name.as_str()).unwrap_or("X");
                let y_name = self.channels.get(self.xy_ch_y).map(|c| c.name.as_str()).unwrap_or("Y");

                Plot::new("xy_plot")
                    .show_grid([true, true])
                    .x_axis_label(x_name)
                    .y_axis_label(y_name)
                    .allow_zoom(Vec2b::new(true, true))
                    .allow_drag(Vec2b::new(true, true))
                    .height(ui.available_height() - 10.0)
                    .show(ui, |plot_ui| {
                        if !xy_points.is_empty() {
                            let line = Line::new(PlotPoints::from(xy_points))
                                .color(Color32::from_rgb(0, 255, 100))
                                .width(1.0);
                            plot_ui.line(line);
                        }
                    });
            });
    }
}

// ---------- Math channel dialog ----------

impl OscilloscopeApp {
    fn draw_math_dialog(&mut self, ctx: &egui::Context) {
        if !self.show_math_dialog {
            return;
        }
        if self.data.is_none() {
            self.show_math_dialog = false;
            return;
        }

        let n_ch = self.data.as_ref().map(|d| d.n_channels()).unwrap_or(0);
        let n_real = n_ch;
        let math_count = self.math_channels.len();

        let mut should_close = false;

        egui::Window::new("Add Math Channel")
            .open(&mut self.show_math_dialog)
            .default_size([350.0, 250.0])
            .resizable(false)
            .show(ctx, |ui| {
                // Operation selector
                ui.horizontal(|ui| {
                    ui.label("Operation:");
                    egui::ComboBox::from_id_salt("math_op_select")
                        .selected_text(self.math_new_op.to_string())
                        .show_ui(ui, |ui| {
                            for op in MathOp::all() {
                                ui.selectable_value(&mut self.math_new_op, *op, op.to_string());
                            }
                        });
                });

                ui.add_space(4.0);

                // Source A
                ui.horizontal(|ui| {
                    ui.label("Source A:");
                    egui::ComboBox::from_id_salt("math_src_a")
                        .selected_text(
                            self.channels
                                .get(self.math_new_src_a)
                                .map(|c| c.name.as_str())
                                .unwrap_or("---"),
                        )
                        .show_ui(ui, |ui| {
                            for i in 0..n_ch.min(self.channels.len()) {
                                ui.selectable_value(
                                    &mut self.math_new_src_a,
                                    i,
                                    &self.channels[i].name,
                                );
                            }
                        });
                });

                // Source B (only for binary operations)
                if self.math_new_op.needs_source_b() {
                    ui.horizontal(|ui| {
                        ui.label("Source B:");
                        egui::ComboBox::from_id_salt("math_src_b")
                            .selected_text(
                                self.channels
                                    .get(self.math_new_src_b)
                                    .map(|c| c.name.as_str())
                                    .unwrap_or("---"),
                            )
                            .show_ui(ui, |ui| {
                                for i in 0..n_ch.min(self.channels.len()) {
                                    ui.selectable_value(
                                        &mut self.math_new_src_b,
                                        i,
                                        &self.channels[i].name,
                                    );
                                }
                            });
                    });
                }

                ui.add_space(8.0);

                // Preview name
                let channel_names: Vec<String> = self.channels.iter().map(|c| c.name.clone()).collect();
                let preview_def = MathChannelDef {
                    operation: self.math_new_op,
                    source_a: self.math_new_src_a,
                    source_b: if self.math_new_op.needs_source_b() {
                        Some(self.math_new_src_b)
                    } else {
                        None
                    },
                };
                let preview_name = preview_def.display_name(&channel_names);
                ui.label(RichText::new(format!("Result: {}", preview_name)).strong());

                ui.add_space(8.0);

                ui.horizontal(|ui| {
                    if ui.button("Add").clicked() {
                        let def = MathChannelDef {
                            operation: self.math_new_op,
                            source_a: self.math_new_src_a,
                            source_b: if self.math_new_op.needs_source_b() {
                                Some(self.math_new_src_b)
                            } else {
                                None
                            },
                        };

                        let math_idx = self.channels.len();
                        let color = CHANNEL_COLORS[(self.channels.len()) % CHANNEL_COLORS.len()];
                        self.channels.push(ChannelState {
                            name: def.display_name(&channel_names),
                            visible: true,
                            delay: 0.0,
                            color,
                        });
                        self.math_channels.push(def);
                        self.cache.push(None);
                        self.measurement_cache.push(None);

                        self.strips.push(Strip {
                            channel_indices: vec![math_idx],
                            height: 150.0,
                        });

                        self.status_message = format!("Added math channel: {}", self.channels[math_idx].name);
                        should_close = true;
                    }
                    if ui.button("Cancel").clicked() {
                        should_close = true;
                    }
                });

                // Show existing math channels
                if math_count > 0 {
                    ui.add_space(8.0);
                    ui.separator();
                    ui.label(RichText::new("Existing Math Channels").strong());
                    let mut to_remove: Option<usize> = None;
                    for i in 0..math_count {
                        let ch_idx = n_real + i;
                        if ch_idx >= self.channels.len() {
                            continue;
                        }
                        ui.horizontal(|ui| {
                            let ch_name = &self.channels[ch_idx].name;
                            ui.colored_label(self.channels[ch_idx].color, ch_name);
                            if ui.button(RichText::new("×").small()).clicked() {
                                to_remove = Some(i);
                            }
                        });
                    }
                    if let Some(mi) = to_remove {
                        self.pending_math_remove = Some(mi);
                    }
                }
            });

        // Deferred removal (after the window closure to avoid borrow issues)
        if let Some(mi) = self.pending_math_remove.take() {
            self.remove_math_channel(mi);
        }

        if should_close {
            self.show_math_dialog = false;
        }
    }

    fn remove_math_channel(&mut self, math_idx: usize) {
        let Some(ref data) = self.data else { return };
        let n_real = data.n_channels();
        let ch_idx = n_real + math_idx;

        // Remove from all strips
        for strip in &mut self.strips {
            strip.channel_indices.retain(|&i| i != ch_idx);
        }
        self.strips.retain(|s| !s.channel_indices.is_empty());

        // Remove math channel and shift indices
        self.math_channels.remove(math_idx);
        // Remove the channel entry + caches
        if ch_idx < self.channels.len() {
            self.channels.remove(ch_idx);
        }
        if ch_idx < self.cache.len() {
            self.cache.remove(ch_idx);
        }
        if ch_idx < self.measurement_cache.len() {
            self.measurement_cache.remove(ch_idx);
        }
        // Fix strip channel indices (anything > ch_idx shifts down by 1)
        for strip in &mut self.strips {
            for idx in &mut strip.channel_indices {
                if *idx > ch_idx {
                    *idx -= 1;
                }
            }
        }
        self.status_message = "Removed math channel".to_owned();
    }
}

// ---------- math channel cache ----------

impl OscilloscopeApp {
    /// Ensure cache for a math channel (index >= n_real_channels).
    fn ensure_math_cache(&mut self, ch_idx: usize) {
        let Some(ref data) = self.data else { return };
        let n_real = data.n_channels();
        let math_idx = ch_idx - n_real;
        if math_idx >= self.math_channels.len() {
            return;
        }
        if ch_idx >= self.cache.len() || ch_idx >= self.channels.len() {
            return;
        }

        let vis_x_min = self.last_bounds.min()[0];
        let vis_x_max = self.last_bounds.max()[0];
        let delay = self.channels[ch_idx].delay;

        // Check cache validity
        if let Some(ref c) = self.cache[ch_idx] {
            if c.is_valid(vis_x_min, vis_x_max, delay, ch_idx) {
                return;
            }
        }

        let def = &self.math_channels[math_idx];
        let mut points = def.compute(data, vis_x_min, vis_x_max, MAX_DISPLAY_POINTS);

        // Apply delay
        if delay != 0.0 {
            for p in &mut points {
                p[0] += delay;
            }
        }

        self.cache[ch_idx] = Some(StripCache {
            points,
            vis_x_min,
            vis_x_max,
            delay,
            ch_idx,
        });
    }
}
