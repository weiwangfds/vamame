//! Toolbar UI: file open, navigation, cursor mode, export, tool toggles.

use egui::{Color32, RichText};

use crate::cursor::CursorMode;

use super::OscilloscopeApp;

impl OscilloscopeApp {
    pub(crate) fn draw_toolbar(&mut self, ctx: &egui::Context) {
        // Process async file-dialog results returned from background thread
        if let Ok(path) = self.open_file_rx.try_recv() {
            self.load_csv_from_path(&path);
        }
        if let Ok((label, path)) = self.save_file_rx.try_recv() {
            match label.as_str() {
                "csv" => self.handle_export_csv_result(&path),
                "png" => self.handle_export_png_result(ctx, &path),
                _ => {}
            }
        }

        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("Oscilloscope");
                ui.separator();

                if ui.button("Open CSV...").clicked() {
                    self.spawn_open_dialog(ctx);
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
                            self.spawn_export_csv_dialog(ctx);
                            ui.close_menu();
                        }
                        if ui.button("Export PNG...").clicked() {
                            self.spawn_export_png_dialog(ctx);
                            ui.close_menu();
                        }
                    });

                    ui.separator();

                    // Measurement panel toggle
                    ui.toggle_value(&mut self.show_measurement_panel, "Measurements");

                    // Overlay measurements on plot
                    let overlay_label = if self.show_overlay_measurements {
                        "Overlay: ON"
                    } else {
                        "Overlay"
                    };
                    if ui
                        .button(overlay_label)
                        .on_hover_text("Show measurements overlaid on waveform")
                        .clicked()
                    {
                        self.show_overlay_measurements = !self.show_overlay_measurements;
                    }

                    // Measurement gate (use cursor range for measurements)
                    let gate_label = if self.measurement_gate {
                        "Gate: ON"
                    } else {
                        "Gate"
                    };
                    if ui
                        .button(gate_label)
                        .on_hover_text(
                            "Restrict measurements to cursor A-B range (requires vertical cursor)",
                        )
                        .clicked()
                    {
                        self.measurement_gate = !self.measurement_gate;
                    }

                    ui.separator();

                    // FFT
                    if ui.button("FFT").clicked() {
                        self.show_fft_window = true;
                    }

                    // XY Mode
                    if ui.button("XY").clicked() {
                        self.show_xy_window = true;
                    }

                    // Eye Diagram
                    if ui.button("Eye").clicked() {
                        self.show_eye_window = true;
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
