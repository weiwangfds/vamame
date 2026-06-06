//! Data layer backed by Polars.
//!
//! Loads CSV into a Polars `DataFrame` and provides zoom-aware
//! downsampled points for each channel. Two-level strategy:
//!
//! - **Zoomed in** (visible rows ≤ max_points): return ALL original points,
//!   faithfully reproducing the waveform sample-by-sample.
//! - **Zoomed out** (visible rows > max_points): M4 algorithm emits 4 points
//!   per bin (first, min, max, last), preserving peaks and visual shape.
//!
//! Both paths use Polars lazy evaluation so even 100 M+ rows respond quickly.

use polars::prelude::*;

/// Loaded waveform data and metadata.
pub struct WaveformData {
    /// The full Polars DataFrame (kept for lazy operations).
    df: DataFrame,
    /// Column name used as the x-axis (time).
    time_col: String,
    /// Column names of the data channels (excluding the time column).
    data_cols: Vec<String>,
    /// Total number of rows.
    pub n_rows: usize,
    /// Global x-axis range.
    pub x_min: f64,
    pub x_max: f64,
    /// x_max - x_min.
    pub time_span: f64,
}

impl WaveformData {
    /// Load a CSV file. All columns are parsed as Float64.
    /// If the file has >1 column, column 0 is treated as the time axis.
    pub fn load_csv(path: &str) -> Result<Self, String> {
        let df = CsvReadOptions::default()
            .with_has_header(false)
            .try_into_reader_with_file_path(Some(path.into()))
            .map_err(|e| format!("CSV open error: {e}"))?
            .finish()
            .map_err(|e| format!("CSV parse error: {e}"))?;

        Self::from_dataframe(df)
    }

    /// Build from an already-loaded DataFrame (useful for testing).
    fn from_dataframe(mut df: DataFrame) -> Result<Self, String> {
        let n_cols = df.width();
        let n_rows = df.height();

        if n_cols == 0 || n_rows == 0 {
            return Err("File has no data".to_owned());
        }

        let col_names: Vec<String> = df
            .get_column_names()
            .iter()
            .map(|s| s.to_string())
            .collect();

        // Cast all columns to Float64.
        // String columns (e.g. from whitespace-padded CSV values like
        // " 3.0269131E-01") are manually trimmed and parsed via Rust's
        // trim()/parse(), since Polars' cast skips values with leading spaces.
        for (idx, name) in col_names.iter().enumerate() {
            let s = df.column(name).map_err(|e| format!("{e}"))?;
            if s.dtype() == &DataType::String {
                let ca = s.str().map_err(|e| format!("{e}"))?;
                let mut parsed: Float64Chunked = ca
                    .into_iter()
                    .map(|opt| opt.and_then(|v| v.trim().parse::<f64>().ok()))
                    .collect();
                // Preserve the original column name (collect defaults to "").
                parsed.rename(name.into());
                df.replace_column(idx, parsed.into_series())
                    .map_err(|e| format!("{e}"))?;
            } else if s.dtype() != &DataType::Float64 {
                let casted = s
                    .cast(&DataType::Float64)
                    .map_err(|e| format!("Cast error for '{name}': {e}"))?;
                df.replace_column(idx, casted)
                    .map_err(|e| format!("{e}"))?;
            }
        }

        // Determine time column.
        let (time_col, data_cols) = if n_cols > 1 {
            (col_names[0].clone(), col_names[1..].to_vec())
        } else {
            (
                col_names[0].clone(),
                vec![col_names[0].clone()],
            )
        };

        // Compute global x range.
        let time_series = df.column(&time_col).map_err(|e| format!("{e}"))?;
        let ca = time_series
            .f64()
            .map_err(|e| format!("Time column not f64: {e}"))?;
        let x_min = ca.min().ok_or("Empty time column")?;
        let x_max = ca.max().ok_or("Empty time column")?;

        Ok(Self {
            df,
            time_col,
            data_cols,
            n_rows,
            x_min,
            x_max,
            time_span: x_max - x_min,
        })
    }

    /// Number of data channels.
    pub fn n_channels(&self) -> usize {
        self.data_cols.len()
    }

    /// Return `[x+delay, y]` display points for one channel.
    ///
    /// Two-level strategy:
    ///   1. **Exact mode** (visible rows ≤ max_points): return every original
    ///      point — pixel-perfect reproduction of the waveform.
    ///   2. **M4 mode** (visible rows > max_points): bin into `max_points/4`
    ///      buckets, emit first/min/max/last per bin (4 pts/bin), preserving
    ///      peaks and overall shape. This is the standard oscilloscope
    ///      peak-detect algorithm used by Tektronix, Keysight, etc.
    pub fn get_channel_points(
        &self,
        ch_idx: usize,
        delay: f64,
        vis_x_min: f64,
        vis_x_max: f64,
        max_points: usize,
    ) -> Vec<[f64; 2]> {
        if ch_idx >= self.data_cols.len() || vis_x_min >= vis_x_max {
            return Vec::new();
        }

        let data_col = &self.data_cols[ch_idx];
        let time_col = &self.time_col;

        // Shared lazy frame: filter to visible x-range.
        let lf = self
            .df
            .clone()
            .lazy()
            .filter(
                col(time_col)
                    .gt_eq(lit(vis_x_min))
                    .and(col(time_col).lt_eq(lit(vis_x_max))),
            )
            .select([col(time_col), col(data_col)]);

        // --- Step 1: count visible rows to decide strategy ---
        let n_visible = match lf
            .clone()
            .select([col(time_col).count().cast(DataType::Int64).alias("cnt")])
            .collect()
        {
            Ok(df) => df
                .column("cnt")
                .ok()
                .and_then(|c| c.i64().ok())
                .and_then(|ca| ca.get(0))
                .unwrap_or(i64::MAX) as usize,
            Err(e) => {
                eprintln!("Count error: {e}");
                return Vec::new();
            }
        };

        // --- Step 2: exact mode — return ALL visible points ---
        if n_visible <= max_points {
            return self.fetch_all_points(lf, time_col, data_col, delay);
        }

        // --- Step 3: M4 downsample — first/min/max/last per bin ---
        self.m4_downsample(lf, time_col, data_col, delay, vis_x_min, vis_x_max, max_points)
    }

    /// Fetch all visible points (exact reproduction, no downsampling).
    fn fetch_all_points(
        &self,
        lf: LazyFrame,
        time_col: &str,
        data_col: &str,
        delay: f64,
    ) -> Vec<[f64; 2]> {
        let df = match lf
            .sort(
                [time_col],
                SortMultipleOptions::default().with_maintain_order(true),
            )
            .collect()
        {
            Ok(df) => df,
            Err(e) => {
                eprintln!("Full fetch error: {e}");
                return Vec::new();
            }
        };

        let x_ca = match df.column(time_col) {
            Ok(c) => match c.f64() {
                Ok(ca) => ca,
                Err(_) => return Vec::new(),
            },
            _ => return Vec::new(),
        };
        let y_ca = match df.column(data_col) {
            Ok(c) => match c.f64() {
                Ok(ca) => ca,
                Err(_) => return Vec::new(),
            },
            _ => return Vec::new(),
        };

        let n = df.height();
        let mut points = Vec::with_capacity(n);
        for i in 0..n {
            if let (Some(x), Some(y)) = (x_ca.get(i), y_ca.get(i)) {
                points.push([x + delay, y]);
            }
        }
        points
    }

    /// M4 downsample: emit first/min/max/last per bin (4 points per bin).
    ///
    /// This preserves waveform peaks (min/max), maintains visual continuity
    /// between bins (first/last), and is the standard approach used by
    /// professional oscilloscopes.
    fn m4_downsample(
        &self,
        lf: LazyFrame,
        time_col: &str,
        data_col: &str,
        delay: f64,
        vis_x_min: f64,
        vis_x_max: f64,
        max_points: usize,
    ) -> Vec<[f64; 2]> {
        let range = vis_x_max - vis_x_min;
        let n_bins = (max_points / 4).max(1);
        let bin_width = range / n_bins as f64;

        let result = lf
            .select([
                col(time_col),
                col(data_col),
                (((col(time_col) - lit(vis_x_min)) / lit(bin_width))
                    .floor()
                    .cast(DataType::Int32))
                .alias("bin"),
            ])
            .group_by([col("bin")])
            .agg([
                col(time_col).first().alias("x_first"),
                col(time_col).last().alias("x_last"),
                col(data_col).first().alias("y_first"),
                col(data_col).last().alias("y_last"),
                col(data_col).min().alias("y_min"),
                col(data_col).max().alias("y_max"),
            ])
            .sort(
                ["bin"],
                SortMultipleOptions::default().with_maintain_order(true),
            )
            .collect();

        let df = match result {
            Ok(df) => df,
            Err(e) => {
                eprintln!("M4 downsample error for ch: {e}");
                return Vec::new();
            }
        };

        // Extract aggregated columns.
        let x_first = match df.column("x_first") {
            Ok(c) => match c.f64() {
                Ok(ca) => ca,
                Err(e) => {
                    eprintln!("x_first error: {e}");
                    return Vec::new();
                }
            },
            _ => return Vec::new(),
        };
        let x_last = match df.column("x_last") {
            Ok(c) => match c.f64() {
                Ok(ca) => ca,
                Err(e) => {
                    eprintln!("x_last error: {e}");
                    return Vec::new();
                }
            },
            _ => return Vec::new(),
        };
        let y_first = match df.column("y_first") {
            Ok(c) => match c.f64() {
                Ok(ca) => ca,
                Err(e) => {
                    eprintln!("y_first error: {e}");
                    return Vec::new();
                }
            },
            _ => return Vec::new(),
        };
        let y_last = match df.column("y_last") {
            Ok(c) => match c.f64() {
                Ok(ca) => ca,
                Err(e) => {
                    eprintln!("y_last error: {e}");
                    return Vec::new();
                }
            },
            _ => return Vec::new(),
        };
        let y_min = match df.column("y_min") {
            Ok(c) => match c.f64() {
                Ok(ca) => ca,
                Err(e) => {
                    eprintln!("y_min error: {e}");
                    return Vec::new();
                }
            },
            _ => return Vec::new(),
        };
        let y_max = match df.column("y_max") {
            Ok(c) => match c.f64() {
                Ok(ca) => ca,
                Err(e) => {
                    eprintln!("y_max error: {e}");
                    return Vec::new();
                }
            },
            _ => return Vec::new(),
        };

        // Emit M4 points per bin in order: first → min → max → last.
        // Using x_first for min/max x positions: within each bin all x values
        // span at most `bin_width`, which is sub-pixel at display resolution.
        let mut points = Vec::with_capacity(df.height() * 4);
        for i in 0..df.height() {
            let xf = x_first.get(i);
            let xl = x_last.get(i);
            let yf = y_first.get(i);
            let yl = y_last.get(i);
            let mn = y_min.get(i);
            let mx = y_max.get(i);

            if let (Some(xf_v), Some(yf_v)) = (xf, yf) {
                points.push([xf_v + delay, yf_v]);
            }
            if let (Some(xf_v), Some(mn_v), Some(mx_v)) = (xf, mn, mx) {
                if (mn_v - mx_v).abs() > f64::EPSILON {
                    // Emit both min and max to preserve waveform peaks.
                    if mn_v < mx_v {
                        points.push([xf_v + delay, mn_v]);
                        points.push([xf_v + delay, mx_v]);
                    } else {
                        points.push([xf_v + delay, mx_v]);
                        points.push([xf_v + delay, mn_v]);
                    }
                }
            }
            if let (Some(xl_v), Some(yl_v)) = (xl, yl) {
                points.push([xl_v + delay, yl_v]);
            }
        }
        points
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_demo_csv() {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("demoCS0_3.csv");
        let data = WaveformData::load_csv(path.to_str().unwrap()).unwrap();
        assert_eq!(data.n_channels(), 7);
        assert!(data.n_rows > 0);
        assert!(data.time_span > 0.0);
    }

    #[test]
    fn downsample_produces_points() {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("demoCS0_3.csv");
        let data = WaveformData::load_csv(path.to_str().unwrap()).unwrap();
        let pts = data.get_channel_points(0, 0.0, data.x_min, data.x_max, 4000);
        assert!(!pts.is_empty(), "downsample returned 0 points");
        // M4: up to 4 pts per bin, max_points/4 bins → ≤ max_points*1.5
        assert!(pts.len() <= 8000, "got {} points", pts.len());
    }

    #[test]
    fn exact_mode_when_zoomed_in() {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("demoCS0_3.csv");
        let data = WaveformData::load_csv(path.to_str().unwrap()).unwrap();
        // Zoom into a tiny range — should return all raw points.
        let mid = (data.x_min + data.x_max) / 2.0;
        let tiny_range = data.time_span / 10000.0;
        let pts = data.get_channel_points(0, 0.0, mid - tiny_range, mid + tiny_range, 4000);
        // With 1281 rows over the full span, a 1/10000 slice has ~0.13 rows,
        // so likely 0 or a few points — just verify it doesn't crash.
        // Use a wider slice that definitely has points.
        let pts = data.get_channel_points(
            0,
            0.0,
            data.x_min,
            data.x_min + data.time_span * 0.1,
            100000, // max_points >> visible rows → exact mode
        );
        assert!(!pts.is_empty(), "exact mode returned 0 points");
    }
}
