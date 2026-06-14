//! XY (Lissajous) mode window.

use egui::{Color32, Vec2b};
use egui_plot::{Line, Plot, PlotPoints};

use super::{OscilloscopeApp, MAX_DISPLAY_POINTS};

impl OscilloscopeApp {
    pub(crate) fn draw_xy_window(&mut self, ctx: &egui::Context) {
        let Some(ref mut data) = self.data else { return };

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

                let x_pts =
                    data.get_raw_points(self.xy_ch_x, vis_x_min, vis_x_max, MAX_DISPLAY_POINTS);
                let y_pts =
                    data.get_raw_points(self.xy_ch_y, vis_x_min, vis_x_max, MAX_DISPLAY_POINTS);

                // Align by minimum length
                let n = x_pts.len().min(y_pts.len());
                let xy_points: Vec<[f64; 2]> = (0..n)
                    .map(|i| [x_pts[i][1], y_pts[i][1]])
                    .collect();

                let x_name = self
                    .channels
                    .get(self.xy_ch_x)
                    .map(|c| c.name.as_str())
                    .unwrap_or("X");
                let y_name = self
                    .channels
                    .get(self.xy_ch_y)
                    .map(|c| c.name.as_str())
                    .unwrap_or("Y");

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
