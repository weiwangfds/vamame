//! Oscilloscope waveform viewer.
//!
//! Run: `cargo run -- /path/to/file.csv`

mod app;
mod cursor;
mod data;
mod export;
mod fft_analysis;
mod math_channel;
mod measurement;

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1400.0, 800.0])
            .with_min_inner_size([800.0, 500.0])
            .with_title("Oscilloscope"),
        ..Default::default()
    };

    // Check for CSV path as command-line argument
    let csv_path = std::env::args().nth(1);

    eframe::run_native(
        "Oscilloscope",
        options,
        Box::new(move |cc| {
            let mut app = app::OscilloscopeApp::default();
            if let Some(ref path) = csv_path {
                // Schedule load on first frame via status message
                app.pending_load_path = Some(path.clone());
            }
            let _ = cc;
            Ok(Box::new(app))
        }),
    )
}
