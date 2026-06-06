//! Main application state and UI rendering.
//!
//! CSV-only static waveform viewer backed by Polars. Each data channel occupies
//! a vertical strip with independently adjustable height. Drag a channel label
//! onto another strip to merge them. All strips share a linked x-axis for
//! synchronised zoom (scroll) and pan (drag). Zoom-aware min/max downsampling
//! keeps interactions fast even with 100 M+ rows.

use egui::{Color32, CursorIcon, Frame, Id, RichText, Sense, Vec2b};
use egui_plot::{Corner, Legend, Line, Plot, PlotBounds, PlotPoints};

use crate::data::WaveformData;

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

    /// File path display.
    loaded_path: String,

    status_message: String,
}

impl Default for OscilloscopeApp {
    fn default() -> Self {
        Self {
            channels: Vec::new(),
            strips: Vec::new(),
            data: None,
            cache: Vec::new(),
            last_bounds: PlotBounds::NOTHING,
            loaded_path: String::new(),
            status_message: "No data loaded — click \"Open CSV...\" to import".to_owned(),
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
                        channel_indices: vec![0], // placeholder, set properly below
                        height: 150.0,
                    })
                    .collect();
                for (i, s) in self.strips.iter_mut().enumerate() {
                    s.channel_indices = vec![i];
                }

                self.cache = vec![None; n_data];

                let wd = self.data.as_ref().unwrap();
                self.last_bounds = PlotBounds::from_min_max(
                    [wd.x_min, -1.0],
                    [wd.x_max, 1.0],
                );

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
        if ch_idx >= self.channels.len() || ch_idx >= self.cache.len() {
            return;
        }

        let vis_x_min = self.last_bounds.min()[0];
        let vis_x_max = self.last_bounds.max()[0];
        let delay = self.channels[ch_idx].delay;

        // Check cache validity.
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
}

// ---------- eframe::App ----------

impl eframe::App for OscilloscopeApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.draw_toolbar(ctx);
        self.draw_central(ctx);
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

                if !self.loaded_path.is_empty() {
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

                        // -- header --
                        ui.horizontal_wrapped(|ui| {
                            ui.label(
                                RichText::new(format!("Strip {}", s_idx + 1))
                                    .small()
                                    .strong(),
                            );
                            ui.separator();

                            let strip = &self.strips[s_idx];
                            for &ch_idx in &strip.channel_indices {
                                if ch_idx >= self.channels.len() {
                                    continue;
                                }
                                let ch = &self.channels[ch_idx];

                                let drag_id = ui.id().with("ch_drag").with(ch_idx);
                                ui.dnd_drag_source(
                                    drag_id,
                                    DragPayload {
                                        channel_idx: ch_idx,
                                        source_strip: s_idx,
                                    },
                                    |ui| {
                                        ui.colored_label(ch.color, &ch.name);
                                    },
                                );

                                ui.label(RichText::new("delay:").small().color(Color32::GRAY));
                                let max_delay = time_span * 0.5;
                                let ch = &mut self.channels[ch_idx];
                                ui.add(
                                    egui::DragValue::new(&mut ch.delay)
                                        .range(-max_delay..=max_delay)
                                        .speed(time_span * 0.001)
                                        .fixed_decimals(2)
                                        .suffix("s"),
                                );
                                ui.separator();
                            }

                            if strip.channel_indices.len() > 1 {
                                if ui.button("Split").clicked() {
                                    split_requested = true;
                                }
                            }
                        });

                        // -- ensure cache --
                        let strip_chs = self.strips[s_idx].channel_indices.clone();
                        for &ch_idx in &strip_chs {
                            self.ensure_cache(ch_idx);
                        }

                        // -- plot --
                        let frame = Frame::default()
                            .stroke(egui::Stroke::new(
                                1.0,
                                Color32::from_rgba_unmultiplied(255, 255, 255, 30),
                            ))
                            .inner_margin(2.0);

                        let (_, dropped) = ui.dnd_drop_zone::<DragPayload, _>(frame, |ui| {
                            let plot_id = ui.id().with("strip_plot").with(s_idx);
                            let show_x_axis = s_idx == self.strips.len() - 1;

                            let plot = Plot::new(plot_id)
                                .legend(Legend::default().position(Corner::RightTop))
                                .show_axes(Vec2b::new(show_x_axis, true))
                                .show_grid([true, true])
                                .link_axis(link_id, Vec2b::new(true, false))
                                // All interactions: horizontal only, Y auto-fits.
                                .allow_zoom(Vec2b::new(true, false))
                                .allow_scroll(Vec2b::new(true, false))
                                .allow_drag(Vec2b::new(true, false))
                                .x_axis_label(if show_x_axis { "Time (s)" } else { "" })
                                .y_axis_label("V")
                                .height(strip_height);

                            let plot_response = plot.show(ui, |plot_ui| {
                                // Y auto-fit: let egui_plot compute Y from rendered data.
                                // Do NOT use set_plot_bounds here — it reads stale X from
                                // the previous frame and overrides the linked-axis X update.
                                plot_ui.set_auto_bounds(Vec2b::new(false, true));

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
                            });

                            // Capture plot bounds for zoom-aware resampling on next frame.
                            let bounds = plot_response.transform.bounds();
                            self.last_bounds = *bounds;
                        });

                        if let Some(payload) = dropped {
                            if payload.source_strip != s_idx {
                                self.move_channel_to_strip(payload.channel_idx, s_idx);
                            }
                        }

                        if split_requested {
                            self.split_strip(s_idx);
                        }

                        // -- resize handle --
                        if s_idx < self.strips.len() - 1 {
                            self.draw_resize_handle(ui, s_idx);
                        } else {
                            // Last strip: allow resizing by dragging bottom edge.
                            self.draw_last_resize_handle(ui, s_idx);
                        }
                    }
                });
        });
    }

    /// Draggable divider between strip `above` and the one below it.
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

    /// Resize handle for the last strip — adjusts only its own height.
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
                    height: h, // same height as parent
                },
            );
        }
        self.status_message = format!("Split strip {} into {} strips", s_idx + 1, chs.len());
    }
}
