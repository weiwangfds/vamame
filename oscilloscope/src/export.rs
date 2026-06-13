//! Export utilities: CSV data export and PNG screenshot.

use crate::data::WaveformData;

/// Export visible-range data for selected channels as CSV.
pub fn export_csv(
    data: &mut WaveformData,
    ch_indices: &[usize],
    vis_x_min: f64,
    vis_x_max: f64,
    path: &str,
) -> Result<(), String> {
    use std::io::Write;

    let mut file = std::fs::File::create(path).map_err(|e| format!("File create error: {e}"))?;

    // Write header
    write!(file, "{}", data.time_col()).map_err(|e| format!("Write error: {e}"))?;
    for &ch_idx in ch_indices {
        if let Some(name) = data.data_cols().get(ch_idx) {
            write!(file, ",{}", name).map_err(|e| format!("Write error: {e}"))?;
        }
    }
    writeln!(file).map_err(|e| format!("Write error: {e}"))?;

    // Fetch raw points for each channel
    let max_points = 10_000_000;
    let mut all_points: Vec<Vec<[f64; 2]>> = Vec::new();
    for &ch_idx in ch_indices {
        let pts = data.get_raw_points(ch_idx, vis_x_min, vis_x_max, max_points);
        all_points.push(pts);
    }

    if all_points.is_empty() || all_points[0].is_empty() {
        return Ok(());
    }

    // Write rows (use first channel's timestamps)
    let n_rows = all_points.iter().map(|p| p.len()).min().unwrap_or(0);
    for i in 0..n_rows {
        write!(file, "{:.15e}", all_points[0][i][0])
            .map_err(|e| format!("Write error: {e}"))?;
        for ch_pts in &all_points {
            write!(file, ",{:.15e}", ch_pts[i][1])
                .map_err(|e| format!("Write error: {e}"))?;
        }
        writeln!(file).map_err(|e| format!("Write error: {e}"))?;
    }

    Ok(())
}

/// Save an egui `ColorImage` as a PNG file.
pub fn save_png(image: &egui::ColorImage, path: &str) -> Result<(), String> {
    let w = image.size[0] as u32;
    let h = image.size[1] as u32;
    let mut img_buf = image::ImageBuffer::<image::Rgba<u8>, Vec<u8>>::new(w, h);

    for (i, pixel) in image.pixels.iter().enumerate() {
        let x = i % image.size[0];
        let y = i / image.size[0];
        let [r, g, b, a] = pixel.to_array();
        img_buf.put_pixel(x as u32, y as u32, image::Rgba([r, g, b, a]));
    }

    img_buf
        .save(path)
        .map_err(|e| format!("PNG save error: {e}"))?;
    Ok(())
}
