//! Export utilities: CSV data export and PNG screenshot.

use crate::data::WaveformData;

/// Export visible-range data for selected channels as CSV.
pub fn export_csv(
    data: &WaveformData,
    ch_indices: &[usize],
    vis_x_min: f64,
    vis_x_max: f64,
    path: &str,
) -> Result<(), String> {
    use polars::prelude::*;

    let time_col = data.time_col();
    let mut cols: Vec<Expr> = vec![col(time_col)];
    for &ch_idx in ch_indices {
        if let Some(name) = data.data_cols().get(ch_idx) {
            cols.push(col(name));
        }
    }

    let df = data
        .df()
        .clone()
        .lazy()
        .filter(
            col(time_col)
                .gt_eq(lit(vis_x_min))
                .and(col(time_col).lt_eq(lit(vis_x_max))),
        )
        .select(cols)
        .sort(
            [time_col],
            SortMultipleOptions::default().with_maintain_order(true),
        )
        .collect()
        .map_err(|e| format!("Export query error: {e}"))?;

    let mut file = std::fs::File::create(path).map_err(|e| format!("File create error: {e}"))?;
    CsvWriter::new(&mut file)
        .include_header(true)
        .finish(&mut df.clone())
        .map_err(|e| format!("CSV write error: {e}"))?;

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
