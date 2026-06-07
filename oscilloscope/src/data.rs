//! Data layer — unified interface backed by cached Parquet.
//!
//! On first open, the CSV is converted to Parquet (stored in a `.oscv/`
//! directory next to the source file).  Subsequent opens validate the
//! cache via an MD5 fingerprint and load instantly.  All queries use
//! `scan_parquet` with predicate pushdown so only the relevant row groups
//! are read from disk, keeping memory usage constant.

pub mod cache;
pub mod indexed;

use polars::prelude::*;

use cache::CacheMeta;

/// Loaded waveform data — either cached (Parquet) or legacy (in-memory).
pub enum WaveformData {
    /// Parquet-backed with lazy scan queries.
    Parquet(ParquetData),
    /// Legacy Polars in-memory (small files, fallback).
    InMemory(InMemoryData),
}

impl WaveformData {
    // ── Public accessors ──

    pub fn n_channels(&self) -> usize {
        match self {
            Self::Parquet(d) => d.meta.n_channels(),
            Self::InMemory(d) => d.data_cols.len(),
        }
    }

    pub fn n_rows(&self) -> usize {
        match self {
            Self::Parquet(d) => d.meta.n_rows,
            Self::InMemory(d) => d.n_rows,
        }
    }

    pub fn x_min(&self) -> f64 {
        match self {
            Self::Parquet(d) => d.meta.x_min,
            Self::InMemory(d) => d.x_min,
        }
    }

    pub fn x_max(&self) -> f64 {
        match self {
            Self::Parquet(d) => d.meta.x_max,
            Self::InMemory(d) => d.x_max,
        }
    }

    pub fn time_span(&self) -> f64 {
        self.x_max() - self.x_min()
    }

    pub fn time_col(&self) -> &str {
        match self {
            Self::Parquet(d) => &d.time_col,
            Self::InMemory(d) => &d.time_col,
        }
    }

    pub fn data_cols(&self) -> &[String] {
        match self {
            Self::Parquet(d) => &d.data_cols,
            Self::InMemory(d) => &d.data_cols,
        }
    }

    pub fn df(&self) -> &DataFrame {
        match self {
            Self::Parquet(_) => {
                static EMPTY: std::sync::OnceLock<DataFrame> = std::sync::OnceLock::new();
                EMPTY.get_or_init(|| DataFrame::empty())
            }
            Self::InMemory(d) => &d.df,
        }
    }

    // ── Queries ──

    pub fn get_channel_points(
        &self,
        ch_idx: usize,
        delay: f64,
        vis_x_min: f64,
        vis_x_max: f64,
        max_points: usize,
    ) -> Vec<[f64; 2]> {
        match self {
            Self::Parquet(d) => d.get_channel_points(ch_idx, delay, vis_x_min, vis_x_max, max_points),
            Self::InMemory(d) => d.get_channel_points(ch_idx, delay, vis_x_min, vis_x_max, max_points),
        }
    }

    /// Compute min/max/mean/rms/count directly from Parquet via aggregations.
    /// Returns (vmin, vmax, vmean, vrms, count) or None on error.
    pub fn compute_channel_stats(
        &self,
        ch_idx: usize,
        vis_x_min: f64,
        vis_x_max: f64,
    ) -> Option<(f64, f64, f64, f64, usize)> {
        match self {
            Self::Parquet(d) => d.compute_channel_stats(ch_idx, vis_x_min, vis_x_max),
            Self::InMemory(_) => None,
        }
    }

    pub fn get_raw_points(
        &self,
        ch_idx: usize,
        vis_x_min: f64,
        vis_x_max: f64,
        max_points: usize,
    ) -> Vec<[f64; 2]> {
        match self {
            Self::Parquet(d) => d.get_raw_points(ch_idx, vis_x_min, vis_x_max, max_points),
            Self::InMemory(d) => d.get_raw_points(ch_idx, vis_x_min, vis_x_max, max_points),
        }
    }

    // ── Loading ──

    /// Open a CSV file.
    ///
    /// 1. Check for `.oscv/` cache → if valid, use Parquet path
    /// 2. Otherwise, convert CSV → Parquet in background, then use Parquet path
    /// 3. Progress callback: `(rows_done, bytes_done, total_bytes)`
    pub fn load_csv(path: &str, progress: &dyn Fn(usize, u64, u64)) -> Result<Self, String> {
        let csv_path = std::path::Path::new(path);

        // Try loading cached metadata
        if let Some(meta) = cache::load_meta(csv_path) {
            eprintln!(
                "Parquet cache HIT: {} rows, {} cols, range [{:.6e}, {:.6e}]",
                meta.n_rows, meta.n_cols, meta.x_min, meta.x_max,
            );
            return Ok(Self::Parquet(ParquetData::from_meta(meta, csv_path)));
        }

        // No cache — convert CSV → Parquet
        eprintln!("No Parquet cache found, converting {} …", path);
        let meta = cache::convert_csv_to_parquet(csv_path, progress)?;
        Ok(Self::Parquet(ParquetData::from_meta(meta, csv_path)))
    }
}

// ───────────────────────────────────────────────────────────────────────
// Parquet-backed data (primary path)
// ───────────────────────────────────────────────────────────────────────

pub struct ParquetData {
    pub meta: CacheMeta,
    pub time_col: String,
    pub data_cols: Vec<String>,
    parquet_path: std::path::PathBuf,
}

impl CacheMeta {
    pub fn n_channels(&self) -> usize {
        if self.n_cols > 1 { self.n_cols - 1 } else { 1 }
    }
}

impl ParquetData {
    pub fn from_meta(meta: CacheMeta, csv_path: &std::path::Path) -> Self {
        let time_col = meta.columns[0].clone();
        let data_cols = if meta.n_cols > 1 {
            meta.columns[1..].to_vec()
        } else {
            vec![meta.columns[0].clone()]
        };
        Self {
            meta,
            time_col,
            data_cols,
            parquet_path: cache::parquet_path(csv_path),
        }
    }

    fn lazy_scan(&self) -> LazyFrame {
        LazyFrame::scan_parquet(
            &self.parquet_path as &std::path::Path,
            ScanArgsParquet::default(),
        ).unwrap()
    }

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

        let tc = &self.time_col;
        let dc = &self.data_cols[ch_idx];

        // Estimate visible row count from metadata to avoid an expensive COUNT query.
        // If the visible range covers ≥90% of the full range, use the known total
        // row count directly.  For narrower ranges, assume uniform distribution.
        let full_range = self.meta.x_max - self.meta.x_min;
        let vis_range = vis_x_max - vis_x_min;
        let estimated_n = if full_range > 0.0 && vis_range >= full_range * 0.9 {
            // Full-range (or near-full) view — use exact metadata count.
            self.meta.n_rows
        } else if full_range > 0.0 {
            // Partial view — estimate proportionally (may over-estimate but avoids COUNT).
            ((self.meta.n_rows as f64) * (vis_range / full_range)).ceil() as usize
        } else {
            0
        };

        let lf = self.lazy_scan()
            .filter(
                col(tc).gt_eq(lit(vis_x_min))
                    .and(col(tc).lt_eq(lit(vis_x_max)))
            )
            .select([col(tc), col(dc)]);

        if estimated_n <= max_points {
            self.fetch_exact(lf, delay)
        } else {
            self.m4_downsample(lf, dc, delay, vis_x_min, vis_x_max, max_points)
        }
    }

    pub fn get_raw_points(
        &self,
        ch_idx: usize,
        vis_x_min: f64,
        vis_x_max: f64,
        max_points: usize,
    ) -> Vec<[f64; 2]> {
        if ch_idx >= self.data_cols.len() || vis_x_min >= vis_x_max {
            return Vec::new();
        }

        let tc = &self.time_col;
        let dc = &self.data_cols[ch_idx];

        let lf = self.lazy_scan()
            .filter(
                col(tc).gt_eq(lit(vis_x_min))
                    .and(col(tc).lt_eq(lit(vis_x_max)))
            )
            .select([col(tc), col(dc)]);

        let df = match lf.sort(
            [tc],
            SortMultipleOptions::default().with_maintain_order(true),
        ).collect() {
            Ok(df) => df,
            Err(e) => { eprintln!("Raw points error: {e}"); return Vec::new(); }
        };

        let df = if df.height() > max_points {
            let step = (df.height() / max_points).max(1);
            let cols: Vec<Column> = df.get_columns().iter().map(|c| c.gather_every(step, 0)).collect();
            DataFrame::new(cols).unwrap_or(df)
        } else {
            df
        };

        extract_points(&df, tc, 0.0)
    }

    /// Compute min/max/mean/count directly from Parquet via aggregations.
    /// Much faster than fetching raw points for statistical measurements.
    pub fn compute_channel_stats(
        &self,
        ch_idx: usize,
        vis_x_min: f64,
        vis_x_max: f64,
    ) -> Option<(f64, f64, f64, f64, usize)> {
        if ch_idx >= self.data_cols.len() || vis_x_min >= vis_x_max {
            return None;
        }
        let tc = &self.time_col;
        let dc = &self.data_cols[ch_idx];
        let df = self.lazy_scan()
            .filter(col(tc).gt_eq(lit(vis_x_min)).and(col(tc).lt_eq(lit(vis_x_max))))
            .select([
                col(dc).min().alias("vmin"),
                col(dc).max().alias("vmax"),
                col(dc).mean().alias("vmean"),
                (col(dc).cast(DataType::Float64).pow(2.0)).mean().alias("vmean_sq"),
                col(dc).count().alias("n"),
            ])
            .collect().ok()?;
        let vmin  = df.column("vmin").ok()?.f64().ok()?.get(0)?;
        let vmax  = df.column("vmax").ok()?.f64().ok()?.get(0)?;
        let vmean = df.column("vmean").ok()?.f64().ok()?.get(0)?;
        let vmean_sq = df.column("vmean_sq").ok()?.f64().ok()?.get(0)?;
        let n     = df.column("n").ok()?.idx().ok()?.get(0)? as usize;
        let vrms  = vmean_sq.sqrt();
        Some((vmin, vmax, vmean, vrms, n))
    }

    fn fetch_exact(&self, lf: LazyFrame, delay: f64) -> Vec<[f64; 2]> {
        let tc = &self.time_col;
        let df = match lf.sort(
            [tc],
            SortMultipleOptions::default().with_maintain_order(true),
        ).collect() {
            Ok(df) => df,
            Err(e) => { eprintln!("Exact fetch error: {e}"); return Vec::new(); }
        };
        extract_points(&df, tc, delay)
    }

    fn m4_downsample(
        &self,
        lf: LazyFrame,
        dc: &str,
        delay: f64,
        vis_x_min: f64,
        vis_x_max: f64,
        max_points: usize,
    ) -> Vec<[f64; 2]> {
        let tc = &self.time_col;

        // Use first data column (we already know ch_idx is valid from caller)
        let range = vis_x_max - vis_x_min;
        let n_bins = (max_points / 4).max(1);
        let bin_width = range / n_bins as f64;

        let result = lf
            .select([
                col(tc),
                col(dc),
                (((col(tc) - lit(vis_x_min)) / lit(bin_width))
                    .floor()
                    .cast(DataType::Int32))
                .alias("bin"),
            ])
            .group_by([col("bin")])
            .agg([
                col(tc).first().alias("x_first"),
                col(tc).last().alias("x_last"),
                col(dc).first().alias("y_first"),
                col(dc).last().alias("y_last"),
                col(dc).min().alias("y_min"),
                col(dc).max().alias("y_max"),
            ])
            .sort(["bin"], SortMultipleOptions::default().with_maintain_order(true))
            .collect();

        let df = match result {
            Ok(df) => df,
            Err(e) => { eprintln!("M4 error: {e}"); return Vec::new(); }
        };

        let x_first = df.column("x_first").ok().and_then(|c| c.f64().ok()).unwrap();
        let x_last  = df.column("x_last").ok().and_then(|c| c.f64().ok()).unwrap();
        let y_first = df.column("y_first").ok().and_then(|c| c.f64().ok()).unwrap();
        let y_last  = df.column("y_last").ok().and_then(|c| c.f64().ok()).unwrap();
        let y_min   = df.column("y_min").ok().and_then(|c| c.f64().ok()).unwrap();
        let y_max   = df.column("y_max").ok().and_then(|c| c.f64().ok()).unwrap();

        let mut points = Vec::with_capacity(df.height() * 4);
        for i in 0..df.height() {
            if let (Some(xf), Some(yf)) = (x_first.get(i), y_first.get(i)) {
                points.push([xf + delay, yf]);
            }
            if let (Some(xf), Some(mn), Some(mx)) = (x_first.get(i), y_min.get(i), y_max.get(i)) {
                if (mn - mx).abs() > f64::EPSILON {
                    if mn < mx {
                        points.push([xf + delay, mn]);
                        points.push([xf + delay, mx]);
                    } else {
                        points.push([xf + delay, mx]);
                        points.push([xf + delay, mn]);
                    }
                }
            }
            if let (Some(xl), Some(yl)) = (x_last.get(i), y_last.get(i)) {
                points.push([xl + delay, yl]);
            }
        }
        points
    }
}

fn extract_points(df: &DataFrame, time_col: &str, delay: f64) -> Vec<[f64; 2]> {
    let x_ca = match df.column(time_col) {
        Ok(c) => match c.f64() { Ok(ca) => ca, Err(_) => return Vec::new() },
        _ => return Vec::new(),
    };
    let y_col_name = df.get_column_names().iter()
        .find(|n| n.as_str() != time_col)
        .map(|n| n.as_str())
        .unwrap_or(time_col);
    let y_ca = match df.column(y_col_name) {
        Ok(c) => match c.f64() { Ok(ca) => ca, Err(_) => return Vec::new() },
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

// ───────────────────────────────────────────────────────────────────────
// Legacy in-memory path (fallback for edge cases)
// ───────────────────────────────────────────────────────────────────────

pub struct InMemoryData {
    pub df: DataFrame,
    pub time_col: String,
    pub data_cols: Vec<String>,
    pub n_rows: usize,
    pub x_min: f64,
    pub x_max: f64,
}

impl InMemoryData {
    pub fn get_channel_points(
        &self, _ch_idx: usize, _delay: f64, _vis_x_min: f64, _vis_x_max: f64, _max_points: usize,
    ) -> Vec<[f64; 2]> { Vec::new() }

    pub fn get_raw_points(
        &self, _ch_idx: usize, _vis_x_min: f64, _vis_x_max: f64, _max_points: usize,
    ) -> Vec<[f64; 2]> { Vec::new() }
}

// ───────────────────────────────────────────────────────────────────────
// Tests
// ───────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_demo_csv() {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent().unwrap().join("demoCS0_3.csv");
        if !path.exists() {
            eprintln!("Skipping: demoCS0_3.csv not found");
            return;
        }
        let data = WaveformData::load_csv(path.to_str().unwrap(), &|_, _, _| {}).unwrap();
        assert_eq!(data.n_channels(), 7);
        assert!(data.n_rows() > 0);
        assert!(data.time_span() > 0.0);
    }

    #[test]
    fn downsample_produces_points() {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent().unwrap().join("demoCS0_3.csv");
        if !path.exists() {
            eprintln!("Skipping: demoCS0_3.csv not found");
            return;
        }
        let data = WaveformData::load_csv(path.to_str().unwrap(), &|_, _, _| {}).unwrap();
        let pts = data.get_channel_points(0, 0.0, data.x_min(), data.x_max(), 4000);
        assert!(!pts.is_empty(), "downsample returned 0 points");
        assert!(pts.len() <= 8000, "got {} points", pts.len());
    }

    #[test]
    fn exact_mode_when_zoomed_in() {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent().unwrap().join("demoCS0_3.csv");
        if !path.exists() {
            eprintln!("Skipping: demoCS0_3.csv not found");
            return;
        }
        let data = WaveformData::load_csv(path.to_str().unwrap(), &|_, _, _| {}).unwrap();
        let pts = data.get_channel_points(
            0, 0.0,
            data.x_min(),
            data.x_min() + data.time_span() * 0.1,
            100000,
        );
        assert!(!pts.is_empty(), "exact mode returned 0 points");
    }

    /// Verify that raw_points returns EXACTLY the same values as the CSV.
    /// We cross-check against known values from demoCS0_2.csv at specific rows.
    #[test]
    fn raw_points_match_csv_for_demo2() {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent().unwrap().join("demoCS0_2.csv");

        // Skip if file doesn't exist (CI / other machines).
        if !path.exists() {
            eprintln!("Skipping: demoCS0_2.csv not found");
            return;
        }

        let data = WaveformData::load_csv(path.to_str().unwrap(), &|_, _, _| {}).unwrap();
        assert_eq!(data.n_channels(), 7, "demoCS0_2 should have 7 data channels");

        // Query the full range so we get raw (unsampled) data.
        let pts_ch0 = data.get_raw_points(0, data.x_min(), data.x_max(), 500_000);
        assert!(!pts_ch0.is_empty());

        // --- Check first row (row index 0) ---
        // CSV row 1: -1.32094967E-06, -3.608419E-02, ...
        let tol = 1e-12;
        assert!(
            (pts_ch0[0][0] - (-1.32094967e-06)).abs() < tol,
            "CH0 time[0]: got {:.15e}, expected -1.320949670000000e-06",
            pts_ch0[0][0],
        );
        assert!(
            (pts_ch0[0][1] - (-3.608419e-02)).abs() < tol,
            "CH0 value[0]: got {:.15e}, expected -3.608419000000000e-02",
            pts_ch0[0][1],
        );

        // --- Check CH2 (index 1) first row ---
        // CSV: col3 = -7.65844E-03
        let pts_ch1 = data.get_raw_points(1, data.x_min(), data.x_max(), 500_000);
        assert!(
            (pts_ch1[0][1] - (-7.65844e-03)).abs() < tol,
            "CH1 value[0]: got {:.15e}, expected -7.658440000000000e-03",
            pts_ch1[0][1],
        );

        // --- Check CH3 (index 2) first row ---
        // CSV: col4 = 2.50038E-03
        let pts_ch2 = data.get_raw_points(2, data.x_min(), data.x_max(), 500_000);
        assert!(
            (pts_ch2[0][1] - 2.50038e-03).abs() < tol,
            "CH2 value[0]: got {:.15e}, expected 2.500380000000000e-03",
            pts_ch2[0][1],
        );

        // --- Check last row ---
        // CSV row 128000: time=6.790347e-07, CH1=2.3215561e-01
        let last = pts_ch0.last().unwrap();
        assert!(
            (last[0] - 6.790347e-07).abs() < tol,
            "CH0 time[last]: got {:.15e}, expected 6.790347000000000e-07",
            last[0],
        );
        assert!(
            (last[1] - 2.3215561e-01).abs() < tol,
            "CH0 value[last]: got {:.15e}, expected 2.321556100000000e-01",
            last[1],
        );

        eprintln!(
            "Data validation OK: {} rows, 7 channels, first/last values match CSV",
            data.n_rows(),
        );
    }

    /// Verify per-channel statistics match independently computed values.
    #[test]
    fn channel_stats_match_csv_for_demo2() {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent().unwrap().join("demoCS0_2.csv");
        if !path.exists() {
            eprintln!("Skipping: demoCS0_2.csv not found");
            return;
        }

        let data = WaveformData::load_csv(path.to_str().unwrap(), &|_, _, _| {}).unwrap();

        // Expected min/max from awk analysis of demoCS0_2.csv:
        //   CH1: min=-5.583419e-01, max=5.130230e-01
        //   CH3: min=-1.349486e-01, max=1.759953e-01
        let cases: Vec<(usize, f64, f64)> = vec![
            (0, -5.583419e-01, 5.130230e-01),  // CH1
            (2, -1.349486e-01, 1.759953e-01),  // CH3
        ];

        for (ch_idx, exp_min, exp_max) in cases {
            if let Some((vmin, vmax, _vmean, _vrms, n)) =
                data.compute_channel_stats(ch_idx, data.x_min(), data.x_max())
            {
                assert!(n > 0, "CH{}: no stats returned", ch_idx + 1);
                let tol = 1e-6;
                assert!(
                    (vmin - exp_min).abs() < tol * exp_min.abs().max(1.0),
                    "CH{} vmin: got {:.10e}, expected {:.10e}",
                    ch_idx + 1, vmin, exp_min,
                );
                assert!(
                    (vmax - exp_max).abs() < tol * exp_max.abs().max(1.0),
                    "CH{} vmax: got {:.10e}, expected {:.10e}",
                    ch_idx + 1, vmax, exp_max,
                );
                eprintln!(
                    "CH{} stats OK: vmin={:.6e} vmax={:.6e} n={}",
                    ch_idx + 1, vmin, vmax, n,
                );
            } else {
                panic!("CH{}: compute_channel_stats returned None", ch_idx + 1);
            }
        }
    }
}
