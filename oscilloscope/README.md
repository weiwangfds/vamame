# Oscilloscope

A real-time oscilloscope waveform display application built with **Rust** and **egui**.

## Dependencies

| Crate        | Version | Purpose                          |
|--------------|---------|----------------------------------|
| `eframe`     | 0.31    | Native window & app framework    |
| `egui`       | 0.31    | Immediate-mode GUI               |
| `egui_extras`| 0.31    | Extra widgets / plot support     |
| `csv`        | 1.3     | CSV file parsing                 |

Rust edition: **2021** (MSRV 1.72+).

## Project Structure

```
oscilloscope/
├── Cargo.toml
├── README.md
└── src/
    ├── main.rs         # Entry point, window setup
    ├── app.rs          # OscilloscopeApp state & egui UI
    ├── waveform.rs     # Synthetic waveform generators (sine, sawtooth, square)
    └── csv_loader.rs   # CSV data file loader
```

## Build & Run

```bash
# From this directory:
cd oscilloscope
cargo run

# Or from the workspace root:
cargo run -p oscilloscope
```

## Usage

- **Demo mode** (default): Displays animated sine, cosine, sawtooth, and square waveforms.
  - Adjust frequency, amplitude, and sample count from the toolbar.
- **CSV mode**: Click "CSV Data" in the toolbar to view loaded data (extend `app.rs` to hook up file dialogs or command-line arguments).
- **Channel toggles**: Use the bottom panel checkboxes to show/hide individual channels.

## Extending

- Add new waveform types in `waveform.rs` and wire them into `app.rs`.
- Add a file dialog (e.g. `rfd` crate) for interactive CSV loading.
- Add zoom/pan, cursors, measurements, and FFT analysis in the plot area.
- Add serial-port or network data acquisition in a background thread.
