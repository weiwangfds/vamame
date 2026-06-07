//! Measurement side-panel UI: voltage/time stats and cursor deltas.

use egui::{Color32, RichText};

use crate::cursor::CursorMode;
use crate::measurement::Measurements;

use super::OscilloscopeApp;

impl OscilloscopeApp {
    pub(crate) fn draw_measurement_panel(&mut self, ctx: &egui::Context) {
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
                                meas_row(ui, "T-A", self.cursor.pos_a, "s");
                                meas_row(ui, "T-B", self.cursor.pos_b, "s");
                            }
                            CursorMode::Horizontal => {
                                meas_row(ui, "Delta-V", delta, "V");
                                meas_row(ui, "V-A", self.cursor.pos_a, "V");
                                meas_row(ui, "V-B", self.cursor.pos_b, "V");
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
            ui.label(
                RichText::new(Measurements::format_value(value, unit))
                    .small()
                    .monospace(),
            );
        });
    });
}

fn meas_row_na(ui: &mut egui::Ui, name: &str) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(name).small());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(
                RichText::new("---")
                    .small()
                    .color(Color32::GRAY)
                    .monospace(),
            );
        });
    });
}
