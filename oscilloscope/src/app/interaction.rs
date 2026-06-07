//! User interaction handlers: cursor dragging, strip resize, DnD merge/split.

use egui::{Color32, CursorIcon, Sense};

use crate::cursor::{CursorId, CursorMode};

use super::{OscilloscopeApp, Strip, MIN_STRIP_HEIGHT, CURSOR_HIT_PX};

impl OscilloscopeApp {
    // ---- cursor drag interaction ----

    pub(crate) fn handle_cursor_interaction(
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

    // ---- strip resize handles ----

    pub(crate) fn draw_resize_handle(&mut self, ui: &mut egui::Ui, above: usize) {
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

    pub(crate) fn draw_last_resize_handle(&mut self, ui: &mut egui::Ui, strip: usize) {
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

    // ---- strip management ----

    pub(crate) fn move_channel_to_strip(&mut self, ch_idx: usize, target_strip: usize) {
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

    pub(crate) fn split_strip(&mut self, s_idx: usize) {
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
