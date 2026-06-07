//! Oscilloscope waveform viewer.
//!
//! Import CSV data, view per-channel strips, drag to merge, zoom/pan to inspect.
//! Data layer uses Polars for fast loading and zoom-aware downsampling.
//!
//! Run: `cargo run` from the `oscilloscope/` directory.

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

    eframe::run_native(
        "Oscilloscope",
        options,
        Box::new(|_cc| Ok(Box::new(app::OscilloscopeApp::default()))),
    )
}
