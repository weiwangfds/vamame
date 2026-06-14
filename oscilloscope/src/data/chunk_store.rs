//! Chunked TSZ storage with binary index and LRU cache.
//!
//! Data is split into chunk directories (100K rows each), with one `.tsz`
//! file per channel. Each `.tsz` file is a Gorilla-compressed (timestamp,
//! value) stream. A binary sidecar index maps time ranges to chunk
//! directories for O(log n) lookup. An LRU cache keeps recently accessed
//! chunks in memory as `Vec<f64>` columns, so repeated queries on
//! adjacent views are instant.

use rayon::prelude::*;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// Precomputed per-channel aggregate statistics for one chunk.
///
/// Stored in the binary index so that `compute_channel_stats` can sum these
/// across chunks without decoding any TSZ data. NaN values are excluded
/// (count = number of finite samples).
#[derive(Clone, Debug, Copy)]
#[repr(C)]
#[derive(bytemuck::Pod, bytemuck::Zeroable)]
pub struct ChannelStats {
    pub min: f64,
    pub max: f64,
    pub sum: f64,
    pub sum_sq: f64,
    pub count: u64,
}

impl ChannelStats {
    /// Aggregate for a single chunk's column, skipping NaN/infinite values.
    pub fn from_values(values: &[f64]) -> Self {
        let mut s = Self {
            min: f64::INFINITY,
            max: f64::NEG_INFINITY,
            sum: 0.0,
            sum_sq: 0.0,
            count: 0,
        };
        for &v in values {
            if !v.is_finite() {
                continue;
            }
            if v < s.min { s.min = v; }
            if v > s.max { s.max = v; }
            s.sum += v;
            s.sum_sq += v * v;
            s.count += 1;
        }
        // Normalise the "no finite samples" sentinel so aggregation is safe.
        if s.count == 0 {
            s.min = f64::INFINITY;
            s.max = f64::NEG_INFINITY;
        }
        s
    }

    /// Combine another aggregate into this one (pointwise).
    pub fn merge(&mut self, other: &ChannelStats) {
        if other.count == 0 {
            return;
        }
        if other.min < self.min { self.min = other.min; }
        if other.max > self.max { self.max = other.max; }
        self.sum += other.sum;
        self.sum_sq += other.sum_sq;
        self.count += other.count;
    }
}

/// Metadata for one chunk, stored in the binary index.
#[derive(Clone, Debug)]
pub struct ChunkEntry {
    pub index: u32,
    pub t_min: f64,
    pub t_max: f64,
    pub row_count: u64,
    pub file_size: u64,
    /// Per-channel precomputed stats, indexed by data channel (ch_idx).
    /// Empty for legacy v2 indices (computed on demand).
    pub stats: Vec<ChannelStats>,
}

/// Loaded chunk data held in the LRU cache.
///
/// `columns[0]` is timestamps, `columns[1..]` are voltage channels.
/// All columns are the same length (= chunk row count).
pub struct CachedChunk {
    pub columns: Vec<Vec<f64>>,
    pub t_min: f64,
    pub t_max: f64,
}

/// The chunk store: index + LRU cache + query engine.
pub struct ChunkStore {
    chunks_dir: PathBuf,
    pub entries: Vec<ChunkEntry>,
    pub columns: Vec<String>,
    pub total_rows: usize,
    pub x_min: f64,
    pub x_max: f64,

    cache: HashMap<u32, CachedChunk>,
    access_order: Vec<u32>,
    max_cached: usize,
}

// ─── Binary Index I/O ────────────────────────────────────────────────

const INDEX_MAGIC: &[u8; 8] = b"OSCVIDX\0";
const INDEX_VERSION: u32 = 3;

/// Write the binary index file.
pub fn write_index(path: &Path, entries: &[ChunkEntry], columns: &[String], total_rows: usize, x_min: f64, x_max: f64, rows_per_chunk: u64) -> Result<(), String> {
    let mut f = std::fs::File::create(path)
        .map_err(|e| format!("Cannot create index: {e}"))?;

    f.write_all(INDEX_MAGIC).unwrap();
    f.write_all(&INDEX_VERSION.to_le_bytes()).unwrap();
    f.write_all(&(columns.len() as u32).to_le_bytes()).unwrap();
    f.write_all(&(entries.len() as u32).to_le_bytes()).unwrap();
    f.write_all(&rows_per_chunk.to_le_bytes()).unwrap();
    f.write_all(&(total_rows as u64).to_le_bytes()).unwrap();
    f.write_all(&x_min.to_le_bytes()).unwrap();
    f.write_all(&x_max.to_le_bytes()).unwrap();

    for entry in entries {
        f.write_all(&entry.t_min.to_le_bytes()).unwrap();
        f.write_all(&entry.t_max.to_le_bytes()).unwrap();
        f.write_all(&entry.row_count.to_le_bytes()).unwrap();
        f.write_all(&0u64.to_le_bytes()).unwrap(); // reserved
        f.write_all(&entry.file_size.to_le_bytes()).unwrap();
        // Per-channel precomputed stats (n_channels = columns.len() - 1).
        let stats_bytes: &[u8] = bytemuck::cast_slice(&entry.stats);
        f.write_all(stats_bytes)
            .map_err(|e| format!("Cannot write stats: {e}"))?;
    }

    Ok(())
}

/// Load the binary index file. Returns (entries, columns, total_rows, x_min, x_max, rows_per_chunk).
pub fn load_index(path: &Path) -> Result<(Vec<ChunkEntry>, Vec<String>, usize, f64, f64, u64), String> {
    let mut f = std::fs::File::open(path)
        .map_err(|e| format!("Cannot open index: {e}"))?;

    let mut magic = [0u8; 8];
    f.read_exact(&mut magic).map_err(|e| format!("Index read error: {e}"))?;
    if &magic != INDEX_MAGIC {
        return Err("Invalid index magic bytes".to_owned());
    }

    let read_u32 = |f: &mut std::fs::File| -> Result<u32, String> {
        let mut buf = [0u8; 4];
        f.read_exact(&mut buf).map_err(|e| format!("Index read error: {e}"))?;
        Ok(u32::from_le_bytes(buf))
    };
    let read_u64 = |f: &mut std::fs::File| -> Result<u64, String> {
        let mut buf = [0u8; 8];
        f.read_exact(&mut buf).map_err(|e| format!("Index read error: {e}"))?;
        Ok(u64::from_le_bytes(buf))
    };
    let read_f64 = |f: &mut std::fs::File| -> Result<f64, String> {
        let mut buf = [0u8; 8];
        f.read_exact(&mut buf).map_err(|e| format!("Index read error: {e}"))?;
        Ok(f64::from_le_bytes(buf))
    };

    let version = read_u32(&mut f)?;
    if version != INDEX_VERSION {
        return Err(format!("Unsupported index version: {version}"));
    }

    let n_cols = read_u32(&mut f)? as usize;
    let n_chunks = read_u32(&mut f)? as usize;
    let rows_per_chunk = read_u64(&mut f)?;
    let total_rows = read_u64(&mut f)? as usize;
    let x_min = read_f64(&mut f)?;
    let x_max = read_f64(&mut f)?;

    let columns: Vec<String> = (0..n_cols).map(|i| format!("column_{}", i + 1)).collect();
    let n_channels = if n_cols > 1 { n_cols - 1 } else { 0 };

    let mut entries = Vec::with_capacity(n_chunks);
    for _ in 0..n_chunks {
        let t_min = read_f64(&mut f)?;
        let t_max = read_f64(&mut f)?;
        let row_count = read_u64(&mut f)?;
        let _reserved = read_u64(&mut f)?;
        let file_size = read_u64(&mut f)?;

        // Read per-channel precomputed stats (n_channels × sizeof(ChannelStats)).
        let stats: Vec<ChannelStats> = if n_channels > 0 {
            let mut buf = vec![0u8; n_channels * std::mem::size_of::<ChannelStats>()];
            f.read_exact(&mut buf)
                .map_err(|e| format!("Index stats read error: {e}"))?;
            bytemuck::cast_slice::<u8, ChannelStats>(&buf).to_vec()
        } else {
            Vec::new()
        };

        entries.push(ChunkEntry {
            index: entries.len() as u32,
            t_min,
            t_max,
            row_count,
            file_size,
            stats,
        });
    }

    Ok((entries, columns, total_rows, x_min, x_max, rows_per_chunk))
}

// ─── ChunkStore Implementation ───────────────────────────────────────

impl ChunkStore {
    pub fn new(
        chunks_dir: PathBuf,
        entries: Vec<ChunkEntry>,
        columns: Vec<String>,
        total_rows: usize,
        x_min: f64,
        x_max: f64,
    ) -> Self {
        Self {
            chunks_dir,
            entries,
            columns,
            total_rows,
            x_min,
            x_max,
            cache: HashMap::new(),
            access_order: Vec::new(),
            max_cached: 64,
        }
    }

    /// Number of data channels (excludes time column).
    pub fn n_channels(&self) -> usize {
        if self.columns.len() > 1 { self.columns.len() - 1 } else { 0 }
    }

    /// Directory holding the chunk `.tsz` files (for background decode).
    pub fn chunks_dir(&self) -> &Path {
        &self.chunks_dir
    }

    /// Chunk index entries (for background decode).
    pub fn entries(&self) -> &[ChunkEntry] {
        &self.entries
    }

    /// Find the range of chunk indices that overlap [t_min, t_max].
    fn chunk_range(&self, t_min: f64, t_max: f64) -> (usize, usize) {
        let start = self.entries.partition_point(|e| e.t_max < t_min);
        let end = self.entries.partition_point(|e| e.t_min <= t_max);
        (start, end)
    }

    /// Load a single chunk from TSZ files into the cache.
    fn load_chunk(&mut self, chunk_idx: u32) -> Result<(), String> {
        // Evict if at capacity
        if self.cache.len() >= self.max_cached && !self.cache.contains_key(&chunk_idx) {
            if let Some(lru_key) = self.access_order.first().cloned() {
                self.cache.remove(&lru_key);
                self.access_order.retain(|&k| k != lru_key);
            }
        }

        let chunk_dir = self.chunks_dir.join(format!("chunk_{:06}", chunk_idx));
        let n_channels = self.n_channels();

        // Decode each channel's .tsz file
        let mut columns: Vec<Vec<f64>> = Vec::with_capacity(n_channels + 1);
        let mut timestamps: Option<Vec<f64>> = None;

        for ch in 0..n_channels {
            let tsz_path = chunk_dir.join(format!("ch{}.tsz", ch));
            let data = std::fs::read(&tsz_path)
                .map_err(|e| format!("Cannot read {}: {e}", tsz_path.display()))?;

            let (ts, vs) = super::tsz_codec::decode_channel(&data);

            // Use timestamps from first channel (all channels share the same time column)
            if timestamps.is_none() {
                timestamps = Some(ts);
            }
            columns.push(vs);
        }

        // Insert timestamps as column 0
        if let Some(ts) = timestamps {
            columns.insert(0, ts);
        } else {
            columns.insert(0, Vec::new());
        }

        let entry = &self.entries[chunk_idx as usize];
        self.cache.insert(chunk_idx, CachedChunk {
            columns,
            t_min: entry.t_min,
            t_max: entry.t_max,
        });
        self.access_order.push(chunk_idx);

        Ok(())
    }

    /// Ensure a chunk is loaded, loading from disk if needed.
    fn ensure_loaded(&mut self, chunk_idx: u32) -> Result<(), String> {
        if !self.cache.contains_key(&chunk_idx) {
            self.load_chunk(chunk_idx)?;
        }
        // Move to end of access order (most recently used)
        self.access_order.retain(|&k| k != chunk_idx);
        self.access_order.push(chunk_idx);
        Ok(())
    }

    // ─── Query Methods ──────────────────────────────────────────────

    /// Fetch raw points (sorted by time) for measurements, FFT, XY, etc.
    ///
    /// Memory-bounded: when the overlapping range contains far more samples than
    /// `max_points`, this precomputes a stride from the chunk `row_count`s and
    /// keeps roughly one point per stride while decoding — instead of
    /// materializing every sample into a `Vec` first (which on a 30 GB file at
    /// full range would allocate tens of GB). Points are still collected in
    /// time order, then uniformly thinned if any overflow remains.
    pub fn get_raw_points(
        &mut self,
        ch_idx: usize,
        vis_x_min: f64,
        vis_x_max: f64,
        max_points: usize,
    ) -> Vec<[f64; 2]> {
        let col_idx = ch_idx + 1;
        if col_idx >= self.columns.len() || vis_x_min >= vis_x_max {
            return Vec::new();
        }

        let (start, end) = self.chunk_range(vis_x_min, vis_x_max);
        if start >= end {
            return Vec::new();
        }

        // Estimate total overlapping rows from per-chunk row counts, then derive
        // a stride so we keep ~max_points/2 bins (each bin → min+max = 2 pts).
        let estimated: usize = self.entries[start..end]
            .iter()
            .map(|e| e.row_count as usize)
            .sum();
        let target_bins = (max_points / 2).max(1);
        let stride = if estimated > target_bins {
            (estimated / target_bins).max(1)
        } else {
            1
        };

        let cap = (estimated / stride).saturating_mul(2).min(max_points + 2) + 2;
        let mut points: Vec<[f64; 2]> = Vec::with_capacity(cap);

        // Min/max envelope state (persists across chunk boundaries).
        let mut bin_count: usize = 0;
        let mut bin_t: f64 = 0.0;
        let mut bin_min: f64 = f64::INFINITY;
        let mut bin_max: f64 = f64::NEG_INFINITY;

        for chunk_idx in start..end {
            if self.ensure_loaded(chunk_idx as u32).is_err() { continue; }
            let chunk = &self.cache[&(chunk_idx as u32)];
            let times = &chunk.columns[0];
            let values = &chunk.columns[col_idx];

            for i in 0..times.len() {
                let t = times[i];
                if !(t >= vis_x_min && t <= vis_x_max) { continue; }
                let v = values[i];
                if bin_count == 0 {
                    bin_t = t;
                    bin_min = v;
                    bin_max = v;
                } else {
                    if v < bin_min { bin_min = v; }
                    if v > bin_max { bin_max = v; }
                }
                bin_count += 1;
                if stride == 1 || bin_count >= stride {
                    points.push([bin_t, bin_min]);
                    if bin_max > bin_min {
                        points.push([bin_t, bin_max]);
                    }
                    bin_count = 0;
                }
            }
        }

        // Flush remaining partial bin.
        if bin_count > 0 {
            points.push([bin_t, bin_min]);
            if bin_max > bin_min {
                points.push([bin_t, bin_max]);
            }
        }

        // Safety net: thin any excess (rare — estimate is usually accurate).
        if points.len() > max_points {
            let step = (points.len() / max_points).max(1);
            points = points.into_iter().step_by(step).collect();
        }

        points
    }

    /// Fetch raw points for several channels in one pass.
    ///
    /// Each overlapping chunk is decoded **once** (via `ensure_loaded`) and its
    /// rows are scanned to fill every requested channel's output vector. This
    /// avoids the N× redundant decoding that happens when calling
    /// `get_raw_points` once per channel — on an 8-channel file at full range
    /// that turns ~6.5s of decoding into ~0.8s.
    ///
    /// `channels[i]` maps to `results[i]`. The query range `[vis_x_min,
    /// vis_x_max]` is shared by all channels (callers add per-channel `delay`
    /// to the returned `t` if needed). `max_points` bounds each result.
    pub fn get_raw_points_multi(
        &mut self,
        channels: &[usize],
        vis_x_min: f64,
        vis_x_max: f64,
        max_points: usize,
    ) -> Vec<Vec<[f64; 2]>> {
        if vis_x_min >= vis_x_max || channels.is_empty() {
            return (0..channels.len()).map(|_| Vec::new()).collect();
        }
        // Validate all channels up front (column index = ch_idx + 1).
        let col_idxs: Vec<usize> = channels.iter().map(|&ch| ch + 1).collect();
        if col_idxs.iter().any(|&c| c >= self.columns.len()) {
            return (0..channels.len()).map(|_| Vec::new()).collect();
        }

        let (start, end) = self.chunk_range(vis_x_min, vis_x_max);
        if start >= end {
            return (0..channels.len()).map(|_| Vec::new()).collect();
        }

        // Each bin → min+max = 2 pts, so target half the budget as bins.
        let estimated: usize = self.entries[start..end]
            .iter()
            .map(|e| e.row_count as usize)
            .sum();
        let target_bins = (max_points / 2).max(1);
        let stride = if estimated > target_bins {
            (estimated / target_bins).max(1)
        } else {
            1
        };

        let cap = (estimated / stride).saturating_mul(2).min(max_points + 2) + 2;
        let mut results: Vec<Vec<[f64; 2]>> =
            (0..channels.len()).map(|_| Vec::with_capacity(cap)).collect();

        let nch = channels.len();
        let mut bin_count: usize = 0;
        let mut bin_t: f64 = 0.0;
        let mut bin_min: Vec<f64> = vec![f64::INFINITY; nch];
        let mut bin_max: Vec<f64> = vec![f64::NEG_INFINITY; nch];

        for chunk_idx in start..end {
            if self.ensure_loaded(chunk_idx as u32).is_err() { continue; }
            let chunk = &self.cache[&(chunk_idx as u32)];
            let times = &chunk.columns[0];

            for i in 0..times.len() {
                let t = times[i];
                if !(t >= vis_x_min && t <= vis_x_max) { continue; }

                if bin_count == 0 {
                    bin_t = t;
                    for (j, &col_idx) in col_idxs.iter().enumerate() {
                        let v = chunk.columns[col_idx][i];
                        bin_min[j] = v;
                        bin_max[j] = v;
                    }
                } else {
                    for (j, &col_idx) in col_idxs.iter().enumerate() {
                        let v = chunk.columns[col_idx][i];
                        if v < bin_min[j] { bin_min[j] = v; }
                        if v > bin_max[j] { bin_max[j] = v; }
                    }
                }
                bin_count += 1;

                if stride == 1 || bin_count >= stride {
                    for (j, out) in results.iter_mut().enumerate() {
                        out.push([bin_t, bin_min[j]]);
                        if bin_max[j] > bin_min[j] {
                            out.push([bin_t, bin_max[j]]);
                        }
                    }
                    bin_count = 0;
                }
            }
        }

        // Flush remaining partial bin.
        if bin_count > 0 {
            for (j, out) in results.iter_mut().enumerate() {
                out.push([bin_t, bin_min[j]]);
                if bin_max[j] > bin_min[j] {
                    out.push([bin_t, bin_max[j]]);
                }
            }
        }

        for points in results.iter_mut() {
            if points.len() > max_points {
                let step = (points.len() / max_points).max(1);
                *points = points.iter().copied().step_by(step).collect();
            }
        }

        results
    }

    /// Compute min/max/mean/RMS/count for a channel in a time range.
    ///
    /// Fast path: chunks that lie entirely within `[vis_x_min, vis_x_max]` use
    /// precomputed per-chunk stats (stored in the index) — no decoding at all.
    /// Only the (at most two) boundary chunks that partially overlap the range
    /// are decoded and filtered point-by-point. NaN values are excluded.
    pub fn compute_channel_stats(
        &mut self,
        ch_idx: usize,
        vis_x_min: f64,
        vis_x_max: f64,
    ) -> Option<(f64, f64, f64, f64, usize)> {
        let col_idx = ch_idx + 1;
        if col_idx >= self.columns.len() || vis_x_min >= vis_x_max {
            return None;
        }

        let (start, end) = self.chunk_range(vis_x_min, vis_x_max);
        if start >= end { return None; }

        // Whether precomputed stats are available for this channel.
        let have_stats = self
            .entries
            .get(start)
            .map(|e| e.stats.len() > ch_idx)
            .unwrap_or(false);

        let mut acc = ChannelStats {
            min: f64::INFINITY,
            max: f64::NEG_INFINITY,
            sum: 0.0,
            sum_sq: 0.0,
            count: 0,
        };

        for chunk_idx in start..end {
            let entry = &self.entries[chunk_idx];
            let fully_inside =
                entry.t_min >= vis_x_min && entry.t_max <= vis_x_max;

            if have_stats && fully_inside {
                // Zero-decode aggregation from precomputed stats.
                if let Some(s) = entry.stats.get(ch_idx) {
                    acc.merge(s);
                    continue;
                }
            }

            // Boundary chunk (or no precomputed stats): decode & filter exactly.
            if self.ensure_loaded(chunk_idx as u32).is_err() { continue; }
            let chunk = &self.cache[&(chunk_idx as u32)];
            let times = &chunk.columns[0];
            let values = &chunk.columns[col_idx];

            for i in 0..times.len() {
                let t = times[i];
                if t < vis_x_min || t > vis_x_max { continue; }
                let v = values[i];
                if !v.is_finite() { continue; }
                if v < acc.min { acc.min = v; }
                if v > acc.max { acc.max = v; }
                acc.sum += v;
                acc.sum_sq += v * v;
                acc.count += 1;
            }
        }

        if acc.count < 2 { return None; }
        let mean = acc.sum / acc.count as f64;
        let rms = (acc.sum_sq / acc.count as f64).sqrt();
        Some((acc.min, acc.max, mean, rms, acc.count as usize))
    }
}

// ─── Stateless decode (for background threads) ──────────────────────

/// Read raw points for several channels **without touching any LRU cache**.
///
/// This is a pure function of `(chunks_dir, entries, query)`: it reads each
/// overlapping chunk's `.tsz` files from disk, decodes them, and collects
/// points for all requested channels in a single pass. Because it holds no
/// mutable state, it is safe to call on a background thread while the UI
/// thread continues to use the main `ChunkStore` for other queries.
///
/// `channels[i]` maps to `results[i]`. Each result is bounded by `max_points`
/// via min/max envelope downsampling (same algorithm as
/// `ChunkStore::get_raw_points_multi`).
pub fn read_raw_points_multi_readonly(
    chunks_dir: &Path,
    entries: &[ChunkEntry],
    channels: &[usize],
    vis_x_min: f64,
    vis_x_max: f64,
    max_points: usize,
) -> Vec<Vec<[f64; 2]>> {
    if vis_x_min >= vis_x_max || channels.is_empty() || entries.is_empty() {
        return (0..channels.len()).map(|_| Vec::new()).collect();
    }

    // Mirror ChunkStore::chunk_range (partition_point on entries).
    let start = entries.partition_point(|e| e.t_max < vis_x_min);
    let end = entries.partition_point(|e| e.t_min <= vis_x_max);
    if start >= end {
        return (0..channels.len()).map(|_| Vec::new()).collect();
    }

    let col_idxs: Vec<usize> = channels.iter().map(|&ch| ch + 1).collect();

    let estimated: usize = entries[start..end]
        .iter()
        .map(|e| e.row_count as usize)
        .sum();
    // Each bin → min+max = 2 pts, so target half the budget as bins.
    let target_bins = (max_points / 2).max(1);
    let stride = if estimated > target_bins {
        (estimated / target_bins).max(1)
    } else {
        1
    };

    // Phase 1: Parallel decode — disk I/O + zstd decompression per chunk,
    // distributed across the rayon thread pool (~Ncores× speedup).
    let chunk_data: Vec<Option<(Vec<f64>, Vec<Vec<f64>>)>> = (start..end)
        .into_par_iter()
        .map(|chunk_idx| {
            let dir = chunks_dir.join(format!("chunk_{:06}", chunk_idx as u32));
            let tsz0 = dir.join("ch0.tsz");
            let bytes = std::fs::read(&tsz0).ok()?;
            let (ts_vec, _) = super::tsz_codec::decode_channel(&bytes);
            if ts_vec.is_empty() {
                return None;
            }
            let mut col_vecs = Vec::with_capacity(col_idxs.len());
            for &col_idx in &col_idxs {
                let p = dir.join(format!("ch{}.tsz", col_idx - 1));
                match std::fs::read(&p) {
                    Ok(b) => {
                        let (_, vs) = super::tsz_codec::decode_channel(&b);
                        col_vecs.push(vs);
                    }
                    Err(_) => return None,
                }
            }
            Some((ts_vec, col_vecs))
        })
        .collect();

    // Phase 2: Sequential min/max envelope. Bin state persists across chunks.
    let cap = (estimated / stride).saturating_mul(2).min(max_points + 2) + 2;
    let mut results: Vec<Vec<[f64; 2]>> =
        (0..channels.len()).map(|_| Vec::with_capacity(cap)).collect();

    let nch = channels.len();
    let mut bin_count: usize = 0;
    let mut bin_t: f64 = 0.0;
    let mut bin_min: Vec<f64> = vec![f64::INFINITY; nch];
    let mut bin_max: Vec<f64> = vec![f64::NEG_INFINITY; nch];

    for chunk in chunk_data {
        let Some((ts_vec, col_vecs)) = chunk else { continue; };

        for i in 0..ts_vec.len() {
            let t = ts_vec[i];
            if !(t >= vis_x_min && t <= vis_x_max) { continue; }

            if bin_count == 0 {
                bin_t = t;
                for (j, vals) in col_vecs.iter().enumerate() {
                    let v = vals.get(i).copied().unwrap_or(f64::NAN);
                    bin_min[j] = v;
                    bin_max[j] = v;
                }
            } else {
                for (j, vals) in col_vecs.iter().enumerate() {
                    let v = vals.get(i).copied().unwrap_or(f64::NAN);
                    if v < bin_min[j] { bin_min[j] = v; }
                    if v > bin_max[j] { bin_max[j] = v; }
                }
            }
            bin_count += 1;

            if stride == 1 || bin_count >= stride {
                for (j, out) in results.iter_mut().enumerate() {
                    out.push([bin_t, bin_min[j]]);
                    if bin_max[j] > bin_min[j] {
                        out.push([bin_t, bin_max[j]]);
                    }
                }
                bin_count = 0;
            }
        }
    }

    // Flush remaining partial bin.
    if bin_count > 0 {
        for (j, out) in results.iter_mut().enumerate() {
            out.push([bin_t, bin_min[j]]);
            if bin_max[j] > bin_min[j] {
                out.push([bin_t, bin_max[j]]);
            }
        }
    }

    for points in results.iter_mut() {
        if points.len() > max_points {
            let step = (points.len() / max_points).max(1);
            *points = points.iter().copied().step_by(step).collect();
        }
    }

    results
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_store(entries: Vec<ChunkEntry>, columns: Vec<String>) -> ChunkStore {
        let total_rows: usize = entries.iter().map(|e| e.row_count as usize).sum();
        let x_min = entries.first().map(|e| e.t_min).unwrap_or(0.0);
        let x_max = entries.last().map(|e| e.t_max).unwrap_or(0.0);
        ChunkStore::new(
            std::path::PathBuf::from("/nonexistent"),
            entries,
            columns,
            total_rows,
            x_min,
            x_max,
        )
    }

    #[test]
    fn channel_stats_from_values_skips_nan() {
        // [1.0, NaN, 3.0, inf, -2.0] → finite: [1.0, 3.0, -2.0]
        let vals = vec![1.0, f64::NAN, 3.0, f64::INFINITY, -2.0];
        let s = ChannelStats::from_values(&vals);
        assert_eq!(s.count, 3);
        assert!((s.min - (-2.0)).abs() < 1e-15);
        assert!((s.max - 3.0).abs() < 1e-15);
        assert!((s.sum - 2.0).abs() < 1e-15); // 1 + 3 - 2
        // mean = 2/3, not NaN despite the NaN in input.
        assert!(s.sum.is_finite());
        assert!((s.sum / 3.0 - (2.0 / 3.0)).abs() < 1e-15);
    }

    #[test]
    fn channel_stats_all_nan_has_zero_count() {
        let s = ChannelStats::from_values(&[f64::NAN, f64::NAN]);
        assert_eq!(s.count, 0);
        assert!(s.min.is_infinite() && s.min > 0.0); // +inf sentinel
        assert!(s.max.is_infinite() && s.max < 0.0); // -inf sentinel
    }

    #[test]
    fn channel_stats_merge_combines() {
        let a = ChannelStats::from_values(&[1.0, 2.0]); // min1 max2 sum3 sq5 n2
        let b = ChannelStats::from_values(&[0.5, 4.0]); // min0.5 max4 sum4.5 sq16.25 n2
        let mut acc = a;
        acc.merge(&b);
        assert_eq!(acc.count, 4);
        assert!((acc.min - 0.5).abs() < 1e-15);
        assert!((acc.max - 4.0).abs() < 1e-15);
        assert!((acc.sum - 7.5).abs() < 1e-15);
        assert!((acc.sum_sq - (1.0 + 4.0 + 0.25 + 16.0)).abs() < 1e-15);
    }

    #[test]
    fn channel_stats_merge_ignores_empty() {
        let mut a = ChannelStats::from_values(&[1.0, 2.0]);
        let empty = ChannelStats::from_values(&[f64::NAN]);
        a.merge(&empty);
        assert_eq!(a.count, 2); // unchanged
    }

    /// Index round-trip: written stats must be read back byte-identical.
    #[test]
    fn index_roundtrip_preserves_stats() {
        let dir = tempdir_for_test();
        let path = dir.join("index.bin");

        let stats = vec![
            ChannelStats { min: -0.5, max: 0.5, sum: 1.25, sum_sq: 0.9, count: 5 },
            ChannelStats { min: -1.0, max: 2.0, sum: 3.0, sum_sq: 5.0, count: 3 },
        ];
        let entries = vec![ChunkEntry {
            index: 0,
            t_min: 0.0,
            t_max: 100.0,
            row_count: 5,
            file_size: 1234,
            stats: stats.clone(),
        }];
        let columns = vec!["t".to_string(), "ch1".to_string(), "ch2".to_string()];

        write_index(&path, &entries, &columns, 5, 0.0, 100.0, 100).unwrap();
        let (loaded, loaded_cols, total, xmin, xmax, _rpc) = load_index(&path).unwrap();

        assert_eq!(loaded_cols.len(), 3);
        assert_eq!(total, 5);
        assert!((xmin - 0.0).abs() < 1e-15);
        assert!((xmax - 100.0).abs() < 1e-15);
        assert_eq!(loaded.len(), 1);
        let e = &loaded[0];
        assert_eq!(e.stats.len(), 2);
        for (got, want) in e.stats.iter().zip(stats.iter()) {
            assert!((got.min - want.min).abs() < 1e-15, "min mismatch");
            assert!((got.max - want.max).abs() < 1e-15, "max mismatch");
            assert!((got.sum - want.sum).abs() < 1e-15, "sum mismatch");
            assert!((got.sum_sq - want.sum_sq).abs() < 1e-15, "sum_sq mismatch");
            assert_eq!(got.count, want.count, "count mismatch");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// compute_channel_stats uses precomputed values for fully-contained chunks
    /// (zero decode) — verified by pointing the store at a nonexistent chunks
    /// dir: if it tried to decode, ensure_loaded would fail.
    #[test]
    fn stats_from_precomputed_no_decode() {
        // Single chunk fully inside the query range.
        let entries = vec![ChunkEntry {
            index: 0,
            t_min: 0.0,
            t_max: 100.0,
            row_count: 4,
            file_size: 0,
            stats: vec![ChannelStats {
                min: -1.0, max: 1.0, sum: 0.0, sum_sq: 2.0, count: 4,
            }],
        }];
        let columns = vec!["t".to_string(), "ch1".to_string()];
        let mut store = make_store(entries, columns);

        let r = store.compute_channel_stats(0, 0.0, 100.0).unwrap();
        assert!((r.0 - (-1.0)).abs() < 1e-15); // vmin
        assert!((r.1 - 1.0).abs() < 1e-15);    // vmax
        assert!((r.2 - 0.0).abs() < 1e-15);    // mean = 0/4
        assert!((r.3 - (2.0_f64 / 4.0).sqrt()).abs() < 1e-15); // rms
        assert_eq!(r.4, 4);                     // count
    }

    /// Precomputed stats aggregate across multiple fully-contained chunks.
    #[test]
    fn stats_aggregate_across_chunks() {
        let entries = vec![
            ChunkEntry {
                index: 0, t_min: 0.0, t_max: 50.0, row_count: 2, file_size: 0,
                stats: vec![ChannelStats { min: -1.0, max: 1.0, sum: 0.0, sum_sq: 2.0, count: 2 }],
            },
            ChunkEntry {
                index: 1, t_min: 50.0, t_max: 100.0, row_count: 2, file_size: 0,
                stats: vec![ChannelStats { min: -3.0, max: 2.0, sum: -1.0, sum_sq: 13.0, count: 2 }],
            },
        ];
        let columns = vec!["t".to_string(), "ch1".to_string()];
        let mut store = make_store(entries, columns);

        let r = store.compute_channel_stats(0, 0.0, 100.0).unwrap();
        assert!((r.0 - (-3.0)).abs() < 1e-15); // global min
        assert!((r.1 - 2.0).abs() < 1e-15);    // global max
        assert_eq!(r.4, 4);                     // total count
        // mean = (0 + -1)/4 = -0.25
        assert!((r.2 - (-0.25)).abs() < 1e-15);
        // rms = sqrt((2+13)/4) = sqrt(3.75)
        assert!((r.3 - (3.75_f64).sqrt()).abs() < 1e-15);
    }

    fn tempdir_for_test() -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "oscv_test_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}
