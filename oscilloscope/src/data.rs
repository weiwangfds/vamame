//! Data layer — unified interface backed by Gorilla-compressed TSZ cache.
//!
//! On first open, the CSV is converted to TSZ files (stored in a `.oscv/`
//! directory next to the source file).  Subsequent opens validate the
//! cache via an MD5 fingerprint and load instantly.  All queries operate
//! on in-memory `Vec<f64>` columns with LRU-cached chunk loading.

pub mod cache;
pub mod chunk_store;
pub mod tsz_codec;

use cache::CacheMeta;

/// Loaded waveform data backed by TSZ chunk store.
pub struct TszData {
    pub meta: CacheMeta,
    pub time_col: String,
    pub data_cols: Vec<String>,
    store: chunk_store::ChunkStore,
}

impl TszData {
    pub fn from_meta(meta: CacheMeta, csv_path: &std::path::Path) -> Self {
        let time_col = meta.columns[0].clone();
        let data_cols = if meta.n_cols > 1 {
            meta.columns[1..].to_vec()
        } else {
            vec![meta.columns[0].clone()]
        };

        let idx_path = cache::index_path(csv_path);
        let (entries, _cols, total_rows, x_min, x_max, _rows_per_chunk) =
            chunk_store::load_index(&idx_path)
                .expect("Failed to load chunk index");

        let cdir = cache::chunks_dir(csv_path);
        let store = chunk_store::ChunkStore::new(
            cdir, entries, meta.columns.clone(), total_rows, x_min, x_max,
        );

        Self { meta, time_col, data_cols, store }
    }

    pub fn n_channels(&self) -> usize {
        self.meta.n_channels()
    }

    pub fn n_rows(&self) -> usize {
        self.meta.n_rows
    }

    pub fn x_min(&self) -> f64 {
        self.meta.x_min
    }

    pub fn x_max(&self) -> f64 {
        self.meta.x_max
    }

    pub fn time_span(&self) -> f64 {
        self.x_max() - self.x_min()
    }

    pub fn time_col(&self) -> &str {
        &self.time_col
    }

    pub fn data_cols(&self) -> &[String] {
        &self.data_cols
    }

    /// Directory holding chunk `.tsz` files (for background decode).
    pub fn chunks_dir(&self) -> &std::path::Path {
        self.store.chunks_dir()
    }

    /// Chunk index entries (for background decode).
    pub fn entries(&self) -> &[chunk_store::ChunkEntry] {
        self.store.entries()
    }

    pub fn get_raw_points(
        &mut self,
        ch_idx: usize,
        vis_x_min: f64,
        vis_x_max: f64,
        max_points: usize,
    ) -> Vec<[f64; 2]> {
        self.store.get_raw_points(ch_idx, vis_x_min, vis_x_max, max_points)
    }

    /// Fetch raw points for several channels in one pass (single decode per
    /// chunk). See `ChunkStore::get_raw_points_multi`.
    pub fn get_raw_points_multi(
        &mut self,
        channels: &[usize],
        vis_x_min: f64,
        vis_x_max: f64,
        max_points: usize,
    ) -> Vec<Vec<[f64; 2]>> {
        self.store.get_raw_points_multi(channels, vis_x_min, vis_x_max, max_points)
    }

    pub fn compute_channel_stats(
        &mut self,
        ch_idx: usize,
        vis_x_min: f64,
        vis_x_max: f64,
    ) -> Option<(f64, f64, f64, f64, usize)> {
        self.store.compute_channel_stats(ch_idx, vis_x_min, vis_x_max)
    }
}

/// Unified waveform data container.
pub enum WaveformData {
    Tsz(TszData),
}

impl WaveformData {
    pub fn n_channels(&self) -> usize {
        match self {
            Self::Tsz(d) => d.n_channels(),
        }
    }

    pub fn n_rows(&self) -> usize {
        match self {
            Self::Tsz(d) => d.n_rows(),
        }
    }

    pub fn x_min(&self) -> f64 {
        match self {
            Self::Tsz(d) => d.x_min(),
        }
    }

    pub fn x_max(&self) -> f64 {
        match self {
            Self::Tsz(d) => d.x_max(),
        }
    }

    pub fn time_span(&self) -> f64 {
        self.x_max() - self.x_min()
    }

    pub fn time_col(&self) -> &str {
        match self {
            Self::Tsz(d) => d.time_col(),
        }
    }

    pub fn data_cols(&self) -> &[String] {
        match self {
            Self::Tsz(d) => d.data_cols(),
        }
    }

    /// Directory holding chunk `.tsz` files (for background decode).
    pub fn chunks_dir(&self) -> &std::path::Path {
        match self {
            Self::Tsz(d) => d.chunks_dir(),
        }
    }

    /// Chunk index entries (for background decode).
    pub fn entries(&self) -> &[chunk_store::ChunkEntry] {
        match self {
            Self::Tsz(d) => d.entries(),
        }
    }

    pub fn compute_channel_stats(
        &mut self,
        ch_idx: usize,
        vis_x_min: f64,
        vis_x_max: f64,
    ) -> Option<(f64, f64, f64, f64, usize)> {
        match self {
            Self::Tsz(d) => d.compute_channel_stats(ch_idx, vis_x_min, vis_x_max),
        }
    }

    pub fn get_raw_points(
        &mut self,
        ch_idx: usize,
        vis_x_min: f64,
        vis_x_max: f64,
        max_points: usize,
    ) -> Vec<[f64; 2]> {
        match self {
            Self::Tsz(d) => d.get_raw_points(ch_idx, vis_x_min, vis_x_max, max_points),
        }
    }

    /// Fetch raw points for several channels in one pass (single decode per
    /// chunk). `channels[i]` maps to `results[i]`.
    pub fn get_raw_points_multi(
        &mut self,
        channels: &[usize],
        vis_x_min: f64,
        vis_x_max: f64,
        max_points: usize,
    ) -> Vec<Vec<[f64; 2]>> {
        match self {
            Self::Tsz(d) => d.get_raw_points_multi(channels, vis_x_min, vis_x_max, max_points),
        }
    }

    /// Open a CSV file.
    ///
    /// 1. Check for `.oscv/` cache → if valid, use cached path
    /// 2. Otherwise, convert CSV → TSZ in background, then use cached path
    /// 3. Progress callback: `(rows_done, bytes_done, total_bytes)`
    pub fn load_csv(path: &str, progress: &dyn Fn(usize, u64, u64)) -> Result<Self, String> {
        let csv_path = std::path::Path::new(path);

        // Try loading cached metadata
        if let Some(meta) = cache::load_meta(csv_path) {
            eprintln!(
                "Cache HIT ({}): {} rows, {} cols, range [{:.6e}, {:.6e}]",
                meta.format, meta.n_rows, meta.n_cols, meta.x_min, meta.x_max,
            );
            return Ok(Self::Tsz(TszData::from_meta(meta, csv_path)));
        }

        // No cache — convert CSV → TSZ
        eprintln!("No cache found, converting {} to TSZ …", path);
        let meta = cache::convert_csv_to_tsz(csv_path, progress)?;
        Ok(Self::Tsz(TszData::from_meta(meta, csv_path)))
    }
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
    fn raw_points_match_csv_for_demo2() {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent().unwrap().join("demoCS0_2.csv");

        if !path.exists() {
            eprintln!("Skipping: demoCS0_2.csv not found");
            return;
        }

        let mut data = WaveformData::load_csv(path.to_str().unwrap(), &|_, _, _| {}).unwrap();
        assert_eq!(data.n_channels(), 7, "demoCS0_2 should have 7 data channels");

        let pts_ch0 = data.get_raw_points(0, data.x_min(), data.x_max(), 500_000);
        assert!(!pts_ch0.is_empty());

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

        let pts_ch1 = data.get_raw_points(1, data.x_min(), data.x_max(), 500_000);
        assert!(
            (pts_ch1[0][1] - (-7.65844e-03)).abs() < tol,
            "CH1 value[0]: got {:.15e}, expected -7.658440000000000e-03",
            pts_ch1[0][1],
        );

        let pts_ch2 = data.get_raw_points(2, data.x_min(), data.x_max(), 500_000);
        assert!(
            (pts_ch2[0][1] - 2.50038e-03).abs() < tol,
            "CH2 value[0]: got {:.15e}, expected 2.500380000000000e-03",
            pts_ch2[0][1],
        );

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

    #[test]
    fn channel_stats_match_csv_for_demo2() {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent().unwrap().join("demoCS0_2.csv");
        if !path.exists() {
            eprintln!("Skipping: demoCS0_2.csv not found");
            return;
        }

        let mut data = WaveformData::load_csv(path.to_str().unwrap(), &|_, _, _| {}).unwrap();

        let cases: Vec<(usize, f64, f64)> = vec![
            (0, -5.583419e-01, 5.130230e-01),
            (2, -1.349486e-01, 1.759953e-01),
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

    /// Sub-range stats must equal full-range stats when the query range covers
    /// the whole data (exercises both fully-contained chunks, via precomputed
    /// values, and the two boundary chunks via decode).
    #[test]
    fn stats_full_range_matches_subrange_aggregation() {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent().unwrap().join("demoCS0_2.csv");
        if !path.exists() {
            eprintln!("Skipping: demoCS0_2.csv not found");
            return;
        }

        // Force a fresh conversion so precomputed stats are populated (v3 index).
        let cache_dir = cache::cache_dir(&path);
        if cache_dir.exists() {
            let _ = std::fs::remove_dir_all(&cache_dir);
        }

        let mut data = WaveformData::load_csv(path.to_str().unwrap(), &|_, _, _| {}).unwrap();
        let xmin = data.x_min();
        let xmax = data.x_max();

        // Full range (all chunks fully contained → precomputed path).
        let full = data.compute_channel_stats(0, xmin, xmax).expect("full stats");

        // Slightly-narrowed range: the two boundary chunks now partially overlap
        // and are decoded + filtered exactly, while interior chunks still use
        // precomputed values. The result should be very close to the full-range
        // stats — only the handful of samples exactly on the endpoints differ.
        let eps = data.time_span() * 1e-9;
        let near_full = data.compute_channel_stats(0, xmin + eps, xmax - eps)
            .expect("near-full stats");
        let count_diff = full.4.saturating_sub(near_full.4);
        assert!(
            count_diff <= 10,
            "count dropped by {count_diff} when shrinking range by eps — \
             expected only a few boundary samples to be excluded \
             (full={}, near={})",
            full.4, near_full.4,
        );
        // Min/max are unaffected unless the extremum happens to sit exactly on
        // an endpoint; allow a tiny relative tolerance.
        assert!(
            (full.0 - near_full.0).abs() < 1e-9 * full.0.abs().max(1.0),
            "vmin mismatch: full={:.10e} near={:.10e}", full.0, near_full.0,
        );
        assert!(
            (full.1 - near_full.1).abs() < 1e-9 * full.1.abs().max(1.0),
            "vmax mismatch: full={:.10e} near={:.10e}", full.1, near_full.1,
        );

        eprintln!(
            "subrange stats OK: vmin={:.6e} vmax={:.6e} mean={:.6e} rms={:.6e} n={}",
            full.0, full.1, full.2, full.3, full.4,
        );
    }

    /// `get_raw_points_multi` must return the same points as calling
    /// `get_raw_points` once per channel with the same range/cap.
    #[test]
    fn raw_points_multi_matches_per_channel() {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent().unwrap().join("demoCS0_2.csv");
        if !path.exists() {
            eprintln!("Skipping: demoCS0_2.csv not found");
            return;
        }

        let mut data = WaveformData::load_csv(path.to_str().unwrap(), &|_, _, _| {}).unwrap();
        let xmin = data.x_min();
        let xmax = data.x_max();
        let n_ch = data.n_channels();

        const CAP: usize = 50_000;
        let chs: Vec<usize> = (0..n_ch).collect();

        // Per-channel (fresh store state each call shares the LRU, but results
        // must still be identical to the batch path).
        let per_channel: Vec<Vec<[f64; 2]>> = (0..n_ch)
            .map(|ch| data.get_raw_points(ch, xmin, xmax, CAP))
            .collect();

        // Multi-channel single pass.
        let batched = data.get_raw_points_multi(&chs, xmin, xmax, CAP);

        assert_eq!(per_channel.len(), batched.len(), "channel count mismatch");
        for ch in 0..n_ch {
            let a = &per_channel[ch];
            let b = &batched[ch];
            assert_eq!(a.len(), b.len(), "CH{} point count mismatch: {} vs {}", ch + 1, a.len(), b.len());
            let mismatches = a.iter().zip(b.iter()).take(20).filter(|(p, q)| {
                (p[0] - q[0]).abs() > 1e-12 || (p[1] - q[1]).abs() > 1e-12
            }).count();
            assert_eq!(mismatches, 0, "CH{}: first 20 points differ", ch + 1);
        }
        eprintln!("get_raw_points_multi OK: {} channels × {} points match per-channel", n_ch, batched[0].len());
    }

    /// The stateless background-decode path must return the same points as the
    /// LRU-backed `get_raw_points_multi` (it re-implements the decode+sample
    /// logic without any shared cache).
    #[test]
    fn readonly_decode_matches_lru_path() {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent().unwrap().join("demoCS0_2.csv");
        if !path.exists() {
            eprintln!("Skipping: demoCS0_2.csv not found");
            return;
        }

        let mut data = WaveformData::load_csv(path.to_str().unwrap(), &|_, _, _| {}).unwrap();
        let xmin = data.x_min();
        let xmax = data.x_max();
        let n_ch = data.n_channels();
        let chs: Vec<usize> = (0..n_ch).collect();
        const CAP: usize = 50_000;

        let via_lru = data.get_raw_points_multi(&chs, xmin, xmax, CAP);
        let via_readonly = chunk_store::read_raw_points_multi_readonly(
            data.chunks_dir(),
            data.entries(),
            &chs,
            xmin,
            xmax,
            CAP,
        );

        assert_eq!(via_lru.len(), via_readonly.len(), "channel count mismatch");
        for ch in 0..n_ch {
            let a = &via_lru[ch];
            let b = &via_readonly[ch];
            assert_eq!(a.len(), b.len(), "CH{} point count mismatch: {} vs {}", ch + 1, a.len(), b.len());
            let mismatches = a.iter().zip(b.iter()).take(50).filter(|(p, q)| {
                (p[0] - q[0]).abs() > 1e-12 || (p[1] - q[1]).abs() > 1e-12
            }).count();
            assert_eq!(mismatches, 0, "CH{}: first 50 points differ", ch + 1);
        }
        eprintln!(
            "read_raw_points_multi_readonly OK: {} channels × {} points match LRU path",
            n_ch, via_readonly[0].len(),
        );
    }

    /// Multi-segment parallel path: merged.csv is large enough to be split into
    /// several parse segments. Validates first/last decoded values across the
    /// segment-merge boundary. Skipped when the (non-repo) data file is absent.
    #[test]
    fn raw_points_match_csv_for_merged() {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent().unwrap().join("merged.csv");
        if !path.exists() {
            eprintln!("Skipping: merged.csv not found");
            return;
        }

        // Force a fresh conversion so the multi-segment parallel path runs
        // (rather than a cache hit from a prior run).
        let cache_dir = cache::cache_dir(&path);
        if cache_dir.exists() {
            let _ = std::fs::remove_dir_all(&cache_dir);
        }

        let mut data = WaveformData::load_csv(path.to_str().unwrap(), &|_, _, _| {}).unwrap();
        assert_eq!(data.n_channels(), 8, "merged.csv should have 8 data channels");

        let pts_ch0 = data.get_raw_points(0, data.x_min(), data.x_max(), 5_000_000);
        assert!(!pts_ch0.is_empty(), "merged.csv returned no points");

        // First/last rows of merged.csv:
        //   -2.2419505084E-04, -2.72194E-01, ...
        //    2.7580494916E-04,  3.28061E-01, ...
        let tol = 1e-9;
        assert!(
            (pts_ch0[0][0] - (-2.2419505084e-04)).abs() < tol,
            "merged CH0 time[0]: got {:.12e}, expected -2.2419505084e-04",
            pts_ch0[0][0],
        );
        assert!(
            (pts_ch0[0][1] - (-2.72194e-01)).abs() < tol,
            "merged CH0 value[0]: got {:.12e}, expected -2.72194e-01",
            pts_ch0[0][1],
        );

        let last = pts_ch0.last().unwrap();
        assert!(
            (last[0] - 2.7580494916e-04).abs() < tol,
            "merged CH0 time[last]: got {:.12e}, expected 2.7580494916e-04",
            last[0],
        );
        assert!(
            (last[1] - 3.28061e-01).abs() < tol,
            "merged CH0 value[last]: got {:.12e}, expected 3.28061e-01",
            last[1],
        );

        eprintln!(
            "merged multi-segment validation OK: {} rows, {} points sampled",
            data.n_rows(),
            pts_ch0.len(),
        );
    }
}
