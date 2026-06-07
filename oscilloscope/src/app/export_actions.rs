//! Export actions: CSV export, PNG screenshot, and screenshot event handling.
//!
//! Uses `rfd::AsyncFileDialog` with `pollster::block_on` on a background
//! thread to avoid the macOS sync-modal crash in rfd 0.15
//! (`panel_ffi.rs:81` URL().unwrap() on None).

use crate::export;

use super::OscilloscopeApp;

impl OscilloscopeApp {
    // ── Open file dialog ──

    pub(crate) fn spawn_open_dialog(&mut self, ctx: &egui::Context) {
        let ctx = ctx.clone();
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        self.open_file_rx = rx;

        std::thread::spawn(move || {
            let file = pollster::block_on(
                rfd::AsyncFileDialog::new()
                    .add_filter("CSV", &["csv", "txt"])
                    .add_filter("All", &["*"])
                    .pick_file(),
            );
            if let Some(file) = file {
                let path = file.path().display().to_string();
                let _ = tx.send(path);
                ctx.request_repaint();
            }
        });
    }

    // ── Export CSV dialog ──

    pub(crate) fn spawn_export_csv_dialog(&mut self, ctx: &egui::Context) {
        if self.data.is_none() {
            return;
        }
        let ctx = ctx.clone();
        let (tx, rx) = std::sync::mpsc::channel::<(String, String)>();
        self.save_file_rx = rx;

        std::thread::spawn(move || {
            let file = pollster::block_on(
                rfd::AsyncFileDialog::new()
                    .add_filter("CSV", &["csv"])
                    .set_file_name("export.csv")
                    .save_file(),
            );
            if let Some(file) = file {
                let path = file.path().display().to_string();
                let _ = tx.send(("csv".to_owned(), path));
                ctx.request_repaint();
            }
        });
    }

    // ── Export PNG dialog ──

    pub(crate) fn spawn_export_png_dialog(&mut self, ctx: &egui::Context) {
        let ctx = ctx.clone();
        let (tx, rx) = std::sync::mpsc::channel::<(String, String)>();
        self.save_file_rx = rx;

        std::thread::spawn(move || {
            let file = pollster::block_on(
                rfd::AsyncFileDialog::new()
                    .add_filter("PNG", &["png"])
                    .set_file_name("screenshot.png")
                    .save_file(),
            );
            if let Some(file) = file {
                let path = file.path().display().to_string();
                let _ = tx.send(("png".to_owned(), path));
                ctx.request_repaint();
            }
        });
    }

    // ── Result handlers (called from main thread via toolbar's try_recv) ──

    pub(crate) fn handle_export_csv_result(&mut self, path: &str) {
        let Some(ref data) = self.data else { return };
        let vis_x_min = self.last_bounds.min()[0];
        let vis_x_max = self.last_bounds.max()[0];
        let ch_indices: Vec<usize> = (0..data.n_channels()).collect();
        match export::export_csv(data, &ch_indices, vis_x_min, vis_x_max, path) {
            Ok(()) => {
                self.status_message = format!("CSV exported to {}", path);
            }
            Err(e) => {
                self.status_message = format!("Export error: {}", e);
            }
        }
    }

    pub(crate) fn handle_export_png_result(&mut self, ctx: &egui::Context, path: &str) {
        self.pending_screenshot_path = Some(path.to_owned());
        ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(
            egui::UserData::new("export_png".to_owned()),
        ));
        self.status_message = "Capturing screenshot...".to_owned();
    }

    /// Check for pending screenshot events and save them.
    pub(crate) fn check_screenshot_events(&mut self, ctx: &egui::Context) {
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
            let _ = &self.pending_screenshot_path;
        }
    }
}
