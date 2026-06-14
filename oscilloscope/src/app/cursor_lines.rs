//! Cursor line drawing on plots (vertical or horizontal cursor pairs).

use egui::Color32;
use egui_plot::{Line, PlotPoints};

use crate::cursor::CursorMode;

/// Draw the two cursor reference lines on a plot.
pub(crate) fn draw_cursor_lines(
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
                Line::new("cursor_a", PlotPoints::from(vec![[pos_a, y_min], [pos_a, y_max]]))
                    .color(color_a)
                    .width(1.5),
            );
            plot_ui.line(
                Line::new("cursor_b", PlotPoints::from(vec![[pos_b, y_min], [pos_b, y_max]]))
                    .color(color_b)
                    .width(1.5),
            );
        }
        CursorMode::Horizontal => {
            let x_min = bounds.min()[0] - 1e6;
            let x_max = bounds.max()[0] + 1e6;
            plot_ui.line(
                Line::new("cursor_a", PlotPoints::from(vec![[x_min, pos_a], [x_max, pos_a]]))
                    .color(color_a)
                    .width(1.5),
            );
            plot_ui.line(
                Line::new("cursor_b", PlotPoints::from(vec![[x_min, pos_b], [x_max, pos_b]]))
                    .color(color_b)
                    .width(1.5),
            );
        }
        CursorMode::Off => {}
    }
}
