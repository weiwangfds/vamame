//! FFT spectrum analysis window.

use egui::{Color32, Vec2b};
use egui_plot::{Line, Plot, PlotPoints};

use crate::fft_analysis;

use super::OscilloscopeApp;

impl OscilloscopeApp {
    pub(crate) fn draw_fft_window(&mut self, ctx: &egui::Context) {
        let Some(ref mut data) = self.data else { return };

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
                let span = (vis_x_max - vis_x_min).max(1e-30);
                let tol = span * 1e-6;
                let needs_recompute = match &self.fft_cache {
                    Some((bounds, ch, wt, sc, _)) => {
                        (bounds.min()[0] - vis_x_min).abs() > tol
                            || (bounds.max()[0] - vis_x_max).abs() > tol
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
                            let line = Line::new("FFT", PlotPoints::from(spectrum))
                                .color(Color32::from_rgb(0, 200, 255))
                                .width(1.5);
                            plot_ui.line(line);
                        }
                    });
            });
    }
}
