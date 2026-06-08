//! Math channel dialog UI and math channel cache management.

use egui::RichText;

use crate::math_channel::{MathChannelDef, MathOp};

use super::{OscilloscopeApp, Strip, StripCache, CHANNEL_COLORS, MAX_DISPLAY_POINTS};

impl OscilloscopeApp {
    pub(crate) fn draw_math_dialog(&mut self, ctx: &egui::Context) {
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
                let channel_names: Vec<String> =
                    self.channels.iter().map(|c| c.name.clone()).collect();
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
                        let color =
                            CHANNEL_COLORS[(self.channels.len()) % CHANNEL_COLORS.len()];
                        self.channels.push(super::ChannelState {
                            name: def.display_name(&channel_names),
                            visible: true,
                            delay: 0.0,
                            delay_unit: super::TimeUnit::Ps,
                            color,
                            threshold_enabled: false,
                            threshold_value: 0.0,
                            threshold_unit: super::VoltageUnit::V,
                            binarize_enabled: false,
                            binarize_hide_original: false,
                        });
                        self.math_channels.push(def);
                        self.cache.push(None);
                        self.measurement_cache.push(None);

                        self.strips.push(Strip {
                            channel_indices: vec![math_idx],
                            height: 150.0,
                            y_mode: super::YAxisMode::Linked,
                            y_min: -1.0,
                            y_max: 1.0,
                            y_offset: 0.0,
                        });

                        self.status_message =
                            format!("Added math channel: {}", self.channels[math_idx].name);
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

    // ---- math channel management ----

    pub(crate) fn remove_math_channel(&mut self, math_idx: usize) {
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

    // ---- math channel cache ----

    /// Ensure cache for a math channel (index >= n_real_channels).
    pub(crate) fn ensure_math_cache(&mut self, ch_idx: usize) {
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
