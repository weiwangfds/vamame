//! Central panel: strip-based multi-channel waveform rendering.
//!
//! Each strip holds one or more channels, rendered as overlaid line plots.
//! Strips share a linked x-axis for synchronised zoom/pan.

use egui::{Color32, Frame, Id, RichText, Sense, Vec2, Vec2b};
use egui_plot::{AxisHints, Corner, CoordinatesFormatter, Legend, Line, Plot, PlotImage, PlotPoints, PlotPoint};

use crate::cursor::CursorMode;
use crate::measurement::Measurements;

use super::{
    cursor_lines, DragPayload, OscilloscopeApp, Strip, TimeUnit, VoltageUnit, YAxisMode,
    MIN_STRIP_HEIGHT,
};

impl OscilloscopeApp {
    pub(crate) fn draw_central(&mut self, ctx: &egui::Context) {
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
        let time_span = self.data.as_ref().map(|d| d.time_span()).unwrap_or(1.0);

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
                    // Snapshot navigation flags once before the strip loop so that
                    // ALL strips in this frame see the same value.  The flags are
                    // consumed after the loop (see below).
                    let initial_fit = self.needs_initial_fit;
                    let undo_zoom = self.needs_undo_zoom;
                    let undo_bounds = self.last_bounds;
                    let cursor_mode = self.cursor.mode;
                    let cursor_a = self.cursor.pos_a;
                    let cursor_b = self.cursor.pos_b;

                    // ---- Compute global Y range for Linked mode ----
                    if initial_fit {
                        let mut global_y_min = f64::INFINITY;
                        let mut global_y_max = f64::NEG_INFINITY;
                        for cache_item in &self.cache {
                            if let Some(ref c) = cache_item {
                                for p in &c.points {
                                    if p[1] < global_y_min { global_y_min = p[1]; }
                                    if p[1] > global_y_max { global_y_max = p[1]; }
                                }
                            }
                        }
                        if global_y_min.is_finite() && global_y_max.is_finite() {
                            let margin = (global_y_max - global_y_min) * 0.1;
                            self.y_linked_center = (global_y_min + global_y_max) / 2.0;
                            self.y_linked_half_span = (global_y_max - global_y_min) / 2.0 + margin;
                        }
                    }

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

                                // -- color picker (small swatch, click to change) --
                                {
                                    let c = self.channels[ch_idx].color;
                                    let mut rgb = [
                                        c.r() as f32 / 255.0,
                                        c.g() as f32 / 255.0,
                                        c.b() as f32 / 255.0,
                                    ];
                                    egui::color_picker::color_edit_button_rgb(ui, &mut rgb);
                                    self.channels[ch_idx].color = Color32::from_rgb(
                                        (rgb[0] * 255.0) as u8,
                                        (rgb[1] * 255.0) as u8,
                                        (rgb[2] * 255.0) as u8,
                                    );
                                }

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

                                // -- delay unit selector --
                                ui.label(RichText::new("d:").small().color(Color32::GRAY));
                                {
                                    let unit = self.channels[ch_idx].delay_unit;
                                    let max_delay = time_span * 0.5;
                                    let mut delay_display = unit.from_seconds(self.channels[ch_idx].delay);
                                    let max_display = unit.from_seconds(max_delay);
                                    let speed_display = unit.from_seconds(time_span * 0.001);
                                    ui.add(
                                        egui::DragValue::new(&mut delay_display)
                                            .range(-max_display..=max_display)
                                            .speed(speed_display)
                                            .fixed_decimals(2)
                                            .suffix(unit.suffix()),
                                    );
                                    self.channels[ch_idx].delay = unit.to_seconds(delay_display);

                                    egui::ComboBox::from_id_salt(ui.id().with("delay_unit").with(ch_idx))
                                        .selected_text(unit.suffix())
                                        .width(40.0)
                                        .show_ui(ui, |ui| {
                                            for &u in TimeUnit::all() {
                                                ui.selectable_value(
                                                    &mut self.channels[ch_idx].delay_unit,
                                                    u,
                                                    u.suffix(),
                                                );
                                            }
                                        });
                                }

                                ui.separator();

                                // -- threshold value input with unit selector --
                                ui.label(
                                    RichText::new("Vth:")
                                        .small()
                                        .color(ch_color),
                                );
                                {
                                    let unit = self.channels[ch_idx].threshold_unit;
                                    let mut thresh_display = unit.from_volts(self.channels[ch_idx].threshold_value);
                                    let speed_display = unit.from_volts(0.01);
                                    ui.add(
                                        egui::DragValue::new(&mut thresh_display)
                                            .speed(speed_display)
                                            .fixed_decimals(3)
                                            .suffix(unit.suffix()),
                                    );
                                    self.channels[ch_idx].threshold_value = unit.to_volts(thresh_display);

                                    egui::ComboBox::from_id_salt(ui.id().with("vth_unit").with(ch_idx))
                                        .selected_text(unit.suffix())
                                        .width(40.0)
                                        .show_ui(ui, |ui| {
                                            for &u in VoltageUnit::all() {
                                                ui.selectable_value(
                                                    &mut self.channels[ch_idx].threshold_unit,
                                                    u,
                                                    u.suffix(),
                                                );
                                            }
                                        });
                                }

                                // -- threshold line visibility toggle --
                                let thresh_text = if self.channels[ch_idx].threshold_enabled {
                                    "Line ON"
                                } else {
                                    "Line"
                                };
                                let thresh_color = if self.channels[ch_idx].threshold_enabled {
                                    Color32::from_rgb(255, 100, 100)
                                } else {
                                    Color32::GRAY
                                };
                                if ui
                                    .button(RichText::new(thresh_text).small().color(thresh_color))
                                    .on_hover_text("Show/hide red threshold reference line")
                                    .clicked()
                                {
                                    self.channels[ch_idx].threshold_enabled =
                                        !self.channels[ch_idx].threshold_enabled;
                                }

                                // -- binarize toggle --
                                let bin_text = if self.channels[ch_idx].binarize_enabled {
                                    "0/1 ON"
                                } else {
                                    "0/1"
                                };
                                let bin_color = if self.channels[ch_idx].binarize_enabled {
                                    ch_color
                                } else {
                                    Color32::GRAY
                                };
                                if ui
                                    .button(RichText::new(bin_text).small().color(bin_color))
                                    .on_hover_text("Show binarized square wave")
                                    .clicked()
                                {
                                    self.channels[ch_idx].binarize_enabled =
                                        !self.channels[ch_idx].binarize_enabled;
                                }

                                // -- hide original waveform toggle (only when binarize on) --
                                if self.channels[ch_idx].binarize_enabled {
                                    let hide_text = if self.channels[ch_idx].binarize_hide_original {
                                        "Hide ON"
                                    } else {
                                        "Hide"
                                    };
                                    if ui
                                        .button(RichText::new(hide_text).small())
                                        .on_hover_text("Hide original analog waveform")
                                        .clicked()
                                    {
                                        self.channels[ch_idx].binarize_hide_original =
                                            !self.channels[ch_idx].binarize_hide_original;
                                    }
                                }

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

                            ui.separator();

                            // -- Y-axis controls --
                            let y_mode = self.strips[s_idx].y_mode;

                            let linked_color = if y_mode == YAxisMode::Linked {
                                Color32::from_rgb(100, 180, 255)
                            } else {
                                Color32::GRAY
                            };
                            if ui
                                .button(RichText::new("Y: Linked").small().color(linked_color))
                                .on_hover_text("All strips share the same Y scale")
                                .clicked()
                            {
                                self.strips[s_idx].y_mode = YAxisMode::Linked;
                            }

                            let auto_color = if y_mode == YAxisMode::Auto {
                                Color32::from_rgb(0, 200, 100)
                            } else {
                                Color32::GRAY
                            };
                            if ui
                                .button(RichText::new("Auto").small().color(auto_color))
                                .on_hover_text("Auto-adjust Y range per strip")
                                .clicked()
                            {
                                self.strips[s_idx].y_mode = YAxisMode::Auto;
                            }

                            let manual_color = if y_mode == YAxisMode::Manual {
                                Color32::from_rgb(255, 200, 80)
                            } else {
                                Color32::GRAY
                            };
                            if ui
                                .button(RichText::new("Manual").small().color(manual_color))
                                .on_hover_text("Manually set Y min/max for this strip")
                                .clicked()
                            {
                                self.strips[s_idx].y_mode = YAxisMode::Manual;
                            }

                            if y_mode == YAxisMode::Manual {
                                ui.label(RichText::new("min:").small().color(Color32::GRAY));
                                ui.add(
                                    egui::DragValue::new(&mut self.strips[s_idx].y_min)
                                        .speed(0.01)
                                        .fixed_decimals(3),
                                );
                                ui.label(RichText::new("max:").small().color(Color32::GRAY));
                                ui.add(
                                    egui::DragValue::new(&mut self.strips[s_idx].y_max)
                                        .speed(0.01)
                                        .fixed_decimals(3),
                                );
                            }

                            if y_mode == YAxisMode::Linked {
                                ui.label(RichText::new("offset:").small().color(Color32::GRAY));
                                ui.add(
                                    egui::DragValue::new(&mut self.strips[s_idx].y_offset)
                                        .speed(0.01)
                                        .fixed_decimals(3)
                                        .suffix(" V"),
                                );
                            }
                        });

                        // ======== ensure cache for math channels ========
                        // Real channels are pre-warmed above via the background
                        // ensure_cache_async; only math channels (derived from
                        // others) still need the synchronous path here.
                        let strip_chs = self.strips[s_idx].channel_indices.clone();
                        for &ch_idx in &strip_chs {
                            if let Some(ref data) = self.data {
                                if ch_idx >= data.n_channels() {
                                    self.ensure_cache(ch_idx);
                                }
                            }
                        }

                        // ======== plot ========
                        let frame = Frame::default()
                            .stroke(egui::Stroke::new(
                                1.0,
                                Color32::from_rgba_unmultiplied(255, 255, 255, 30),
                            ))
                            .inner_margin(2.0);

                        let (_, dropped) = ui.dnd_drop_zone::<DragPayload, _>(frame, |ui| {
                            let plot_id = ui.id().with("strip_plot").with(s_idx);
                            let show_x_axis = s_idx == self.strips.len() - 1;

                            // Determine Y-axis control mode for this strip.
                            let strip = &self.strips[s_idx];
                            let y_mode = strip.y_mode;
                            let all_binarize_hide = strip_chs.iter().all(|&ch_idx| {
                                ch_idx < self.channels.len()
                                    && self.channels[ch_idx].binarize_enabled
                                    && self.channels[ch_idx].binarize_hide_original
                            });
                            let y_min_strip = strip.y_min;
                            let y_max_strip = strip.y_max;
                            let y_offset_strip = strip.y_offset;

                            let allow_y_drag = (y_mode == YAxisMode::Linked || y_mode == YAxisMode::Auto)
                                && !all_binarize_hide;

                            let plot = Plot::new(plot_id)
                                .legend(Legend::default().position(Corner::RightTop))
                                .show_axes(Vec2b::new(show_x_axis, true))
                                .show_grid([true, true])
                                .link_axis(link_id, Vec2b::new(true, false))
                                .link_cursor(cursor_link_id, Vec2b::new(true, false))
                                .allow_zoom(Vec2b::new(true, false))
                                .allow_scroll(Vec2b::new(true, false))
                                .allow_drag(Vec2b::new(true, allow_y_drag))
                                .custom_y_axes(vec![
                                    AxisHints::new_y()
                                        .label("V")
                                        .formatter(|mark, _range| {
                                            format_voltage(mark.value)
                                        })
                                ])
                                .y_axis_min_width(80.0)
                                .coordinates_formatter(
                                    Corner::LeftBottom,
                                    CoordinatesFormatter::new(|pt, bounds| {
                                        let x_span = bounds.max()[0] - bounds.min()[0];
                                        let y_span = bounds.max()[1] - bounds.min()[1];
                                        let x_div =
                                            Measurements::format_value(x_span / 10.0, "s/div");
                                        let y_div =
                                            Measurements::format_value(y_span / 8.0, "V/div");
                                        format!(
                                            "t = {:.3e} s  V = {:.6} V\n{}  {}",
                                            pt.x, pt.y, x_div, y_div,
                                        )
                                    }),
                                )
                                .x_axis_label(if show_x_axis { "Time (s)" } else { "" })
                                .height(strip_height);

                            let y_auto_cursor = cursor_mode == CursorMode::Off;
                            let y_offset_before = y_offset_strip;

                            let density_ctx = ui.ctx().clone();
                            let density_w_px = (ui.available_width().max(1.0) as usize).min(2048);
                            let density_h_px = (strip_height.max(1.0) as usize).min(1024);
                            let density_on = self.density_mode;

                            let plot_response = plot.show(ui, |plot_ui| {
                                if initial_fit {
                                    // Auto-fit Y to the data, but keep the X
                                    // range set at load time (which may be a
                                    // sub-window for very large files).
                                    plot_ui.set_auto_bounds(Vec2b::new(false, true));
                                    plot_ui.set_plot_bounds(
                                        egui_plot::PlotBounds::from_min_max(
                                            [self.last_bounds.min()[0], -1.0],
                                            [self.last_bounds.max()[0], 1.0],
                                        ),
                                    );
                                } else if undo_zoom {
                                    plot_ui.set_plot_bounds(undo_bounds);
                                } else if all_binarize_hide {
                                    plot_ui.set_plot_bounds(egui_plot::PlotBounds::from_min_max(
                                        [self.last_bounds.min()[0], -0.1],
                                        [self.last_bounds.max()[0], 1.1],
                                    ));
                                } else if y_mode == YAxisMode::Linked {
                                    let center = self.y_linked_center + y_offset_strip;
                                    let half = self.y_linked_half_span;
                                    plot_ui.set_auto_bounds(Vec2b::new(false, false));
                                    plot_ui.set_plot_bounds(egui_plot::PlotBounds::from_min_max(
                                        [self.last_bounds.min()[0], center - half],
                                        [self.last_bounds.max()[0], center + half],
                                    ));
                                } else if y_mode == YAxisMode::Manual {
                                    plot_ui.set_auto_bounds(Vec2b::new(false, false));
                                    plot_ui.set_plot_bounds(egui_plot::PlotBounds::from_min_max(
                                        [self.last_bounds.min()[0], y_min_strip],
                                        [self.last_bounds.max()[0], y_max_strip],
                                    ));
                                } else if y_mode == YAxisMode::Auto {
                                    plot_ui.set_auto_bounds(Vec2b::new(false, y_auto_cursor));
                                }

                                if density_on {
                                    let b = plot_ui.plot_bounds();
                                    let (dx_min, dx_max) = (b.min()[0], b.max()[0]);
                                    let (dy_min, dy_max) = (b.min()[1], b.max()[1]);
                                    for &ch_idx in &strip_chs {
                                        if ch_idx >= self.channels.len()
                                            || !self.channels[ch_idx].visible
                                            || ch_idx >= self.density_caches.len()
                                        {
                                            continue;
                                        }
                                        let pts: Option<&[[f64; 2]]> =
                                            self.cache.get(ch_idx)
                                                .and_then(|c| c.as_ref())
                                                .map(|c| c.points.as_slice());
                                        let Some(pts) = pts else { continue };
                                        let color = self.channels[ch_idx].color;
                                        if let Some(dr) = self.density_caches[ch_idx]
                                            .ensure_texture(
                                                &density_ctx,
                                                pts,
                                                dx_min, dx_max,
                                                dy_min, dy_max,
                                                density_w_px, density_h_px,
                                                color,
                                            )
                                        {
                                            let img = PlotImage::new(
                                                format!("density-{}", ch_idx),
                                                dr.texture_id,
                                                PlotPoint::new(
                                                    (dr.cached_x_min + dr.cached_x_max) / 2.0,
                                                    (dy_min + dy_max) / 2.0,
                                                ),
                                                Vec2::new(
                                                    (dr.cached_x_max - dr.cached_x_min) as f32,
                                                    (dy_max - dy_min) as f32,
                                                ),
                                            );
                                            plot_ui.image(img);
                                        }
                                    }
                                }

                                for &ch_idx in &strip_chs {
                                    if ch_idx >= self.channels.len()
                                        || !self.channels[ch_idx].visible
                                    {
                                        continue;
                                    }
                                    let ch = &self.channels[ch_idx];

                                    // Draw original analog waveform (unless hidden by binarize)
                                    let hide_original = ch.binarize_enabled && ch.binarize_hide_original;
                                    if !hide_original {
                                        // Line mode: egui_plot::Line (GPU-rasterized)
                                        if let Some(ref cached) = self.cache[ch_idx] {
                                            let line =
                                                Line::new(&ch.name, PlotPoints::from(cached.points.clone()))
                                                    .color(ch.color)
                                                    .width(1.5);
                                            plot_ui.line(line);
                                        }
                                    }

                                    // --- Threshold reference line (red dashed) ---
                                    if ch.threshold_enabled || ch.binarize_enabled {
                                        let bounds = plot_ui.plot_bounds();
                                        let x_min = bounds.min()[0];
                                        let x_max = bounds.max()[0];
                                        let thresh = ch.threshold_value;
                                        plot_ui.line(
                                            Line::new(
                                                format!("{} Vth={:.3}V", ch.name, thresh),
                                                PlotPoints::from(vec![
                                                    [x_min, thresh],
                                                    [x_max, thresh],
                                                ]),
                                            )
                                            .color(
                                                Color32::from_rgba_unmultiplied(255, 80, 80, 200),
                                            )
                                            .width(1.5)
                                            .style(egui_plot::LineStyle::Dashed { length: 6.0 }),
                                        );
                                    }

                                    // --- Binarized square wave ---
                                    if ch.binarize_enabled {
                                        let thresh = ch.threshold_value;
                                        if let Some(ref cached) = self.cache[ch_idx] {
                                            let bin_points =
                                                generate_binarized_points(&cached.points, thresh);
                                            if !bin_points.is_empty() {
                                                plot_ui.line(
                                                    Line::new(
                                                        format!("{} 0/1", ch.name),
                                                        PlotPoints::from(bin_points),
                                                    )
                                                        .color(
                                                            Color32::from_rgba_unmultiplied(
                                                                ch_color_r(ch.color),
                                                                ch_color_g(ch.color),
                                                                ch_color_b(ch.color),
                                                                180,
                                                            ),
                                                        )
                                                        .width(2.0)
                                                        .name(&format!("{} 0/1", ch.name)),
                                                );
                                            }
                                        }
                                    }
                                }

                                // --- Draw cursor lines ---
                                if cursor_mode != CursorMode::Off {
                                    cursor_lines::draw_cursor_lines(
                                        plot_ui, cursor_mode, cursor_a, cursor_b,
                                    );
                                }
                            });

                            // --- Handle cursor drag interaction ---
                            if cursor_mode != CursorMode::Off {
                                self.handle_cursor_interaction(&plot_response, s_idx);
                            }

                            // --- Handle vertical drag for per-strip Y offset ---
                            if y_mode == YAxisMode::Linked && !all_binarize_hide {
                                let response = &plot_response.response;
                                if response.dragged() {
                                    let dy_screen = response.drag_delta().y;
                                    let y_span = self.y_linked_half_span * 2.0;
                                    let plot_height = response.rect.height();
                                    if plot_height > 0.0 {
                                        let dy_data = -dy_screen as f64 * y_span / plot_height as f64;
                                        self.strips[s_idx].y_offset = y_offset_before + dy_data;
                                    }
                                }
                            }

                            let bounds = plot_response.transform.bounds();
                            self.last_bounds = *bounds;

                            // --- Overlay measurement annotations ---
                            if self.show_overlay_measurements {
                                self.draw_overlay_measurements(
                                    ui,
                                    &plot_response,
                                    &strip_chs,
                                    s_idx,
                                );
                            }
                        });

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
                                        y_mode: super::YAxisMode::Linked,
                                        y_min: -1.0,
                                        y_max: 1.0,
                                        y_offset: 0.0,
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

                    // Consume navigation flags after all strips have rendered.
                    self.needs_initial_fit = false;
                    self.needs_undo_zoom = false;
                });
        });
    }

    /// Draw measurement values as overlaid text on the waveform plot.
    fn draw_overlay_measurements(
        &mut self,
        ui: &mut egui::Ui,
        plot_response: &egui_plot::PlotResponse<()>,
        strip_chs: &[usize],
        s_idx: usize,
    ) {
        let plot_rect = plot_response.response.rect;

        for (ch_offset, &ch_idx) in strip_chs.iter().enumerate() {
            if ch_idx >= self.channels.len() || !self.channels[ch_idx].visible {
                continue;
            }
            if ch_idx >= self.measurement_cache.len() {
                continue;
            }
            self.ensure_measurements(ch_idx);

            let Some(Some((_, m))) = self.measurement_cache.get(ch_idx) else {
                continue;
            };

            let ch = &self.channels[ch_idx];

            // Build measurement text lines
            let mut lines: Vec<String> = Vec::new();
            lines.push(format!("{}:", ch.name));
            lines.push(format!("Vpp: {}", Measurements::format_value(m.vpp, "V")));
            lines.push(format!("Vmax: {}", Measurements::format_value(m.vmax, "V")));
            lines.push(format!("Vmin: {}", Measurements::format_value(m.vmin, "V")));
            lines.push(format!("Vmean: {}", Measurements::format_value(m.vmean, "V")));
            lines.push(format!("Vrms: {}", Measurements::format_value(m.vrms, "V")));

            if let Some(freq) = m.frequency {
                lines.push(format!("Freq: {}", Measurements::format_value(freq, "Hz")));
            }
            if let Some(period) = m.period {
                lines.push(format!("Period: {}", Measurements::format_value(period, "s")));
            }

            // Position: offset each channel's annotation block to avoid overlap.
            let x_offset = plot_rect.left() + 8.0 + (ch_offset as f32) * 160.0;
            let y_start = plot_rect.top() + 6.0;

            let font_id = egui::FontId::proportional(11.0);
            let line_height = 14.0;

            for (i, text) in lines.iter().enumerate() {
                let pos = egui::pos2(x_offset, y_start + i as f32 * line_height);
                let galley = ui.painter().layout_no_wrap(text.clone(), font_id.clone(), ch.color);
                let text_rect = egui::Rect::from_min_max(
                    pos,
                    egui::pos2(pos.x + galley.size().x + 4.0, pos.y + galley.size().y + 2.0),
                );
                ui.painter().rect_filled(
                    text_rect,
                    2.0,
                    Color32::from_rgba_unmultiplied(0, 0, 0, 160),
                );
                ui.painter().galley(pos, galley, ch.color);
            }
        }

        let _ = (ui, s_idx);
    }
}

// ---------- helper functions ----------

/// Format a voltage value with automatic unit selection (mV / V).
fn format_voltage(v: f64) -> String {
    let abs = v.abs();
    if abs == 0.0 {
        "0.000 V".to_owned()
    } else if abs < 1.0 {
        format!("{:.3} mV", v * 1e3)
    } else {
        format!("{:.3} V", v)
    }
}

/// Extract the R component from a Color32.
fn ch_color_r(c: Color32) -> u8 {
    c.r()
}

/// Extract the G component from a Color32.
fn ch_color_g(c: Color32) -> u8 {
    c.g()
}

/// Extract the B component from a Color32.
fn ch_color_b(c: Color32) -> u8 {
    c.b()
}

/// Generate a binarized square-wave point sequence from sampled data.
///
/// Uses the threshold to decide HIGH vs LOW:
/// - value > threshold → 1.0 (HIGH)
/// - value ≤ threshold → 0.0 (LOW)
///
/// Vertical edges are inserted at exact crossing times via linear
/// interpolation for clean transitions.
fn generate_binarized_points(points: &[[f64; 2]], threshold: f64) -> Vec<[f64; 2]> {
    if points.len() < 2 {
        return Vec::new();
    }

    let v_high = 1.0;
    let v_low = 0.0;

    let mut result = Vec::with_capacity(points.len() * 2);

    let binarize = |v: f64| -> f64 {
        if v > threshold { v_high } else { v_low }
    };

    let (t0, v0) = (points[0][0], points[0][1]);
    result.push([t0, binarize(v0)]);

    for i in 1..points.len() {
        let (t_prev, v_prev) = (points[i - 1][0], points[i - 1][1]);
        let (t_curr, v_curr) = (points[i][0], points[i][1]);

        let above_prev = v_prev > threshold;
        let above_curr = v_curr > threshold;

        // Insert a vertical edge at the exact crossing time.
        if above_prev != above_curr {
            let dv = v_curr - v_prev;
            if dv.abs() > f64::EPSILON {
                let fraction = (threshold - v_prev) / dv;
                let t_cross = t_prev + fraction * (t_curr - t_prev);
                result.push([t_cross, binarize(v_prev)]);
                result.push([t_cross, binarize(v_curr)]);
            }
        }

        result.push([t_curr, binarize(v_curr)]);
    }

    result
}
