//! Chunked TSZ storage with binary index and LRU cache.
//!
//! Data is split into chunk directories (100K rows each), with one `.tsz`
//! file per channel. Each `.tsz` file is a Gorilla-compressed (timestamp,
//! value) stream. A binary sidecar index maps time ranges to chunk
//! directories for O(log n) lookup. An LRU cache keeps recently accessed
//! chunks in memory as `Vec<f64>` columns, so repeated queries on
//! adjacent views are instant.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// Metadata for one chunk, stored in the binary index.
#[derive(Clone, Debug)]
pub struct ChunkEntry {
    pub index: u32,
    pub t_min: f64,
    pub t_max: f64,
    pub row_count: u64,
    pub file_size: u64,
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

/// Per-bin accumulator for M4 downsampling.
#[derive(Clone)]
struct Bin4 {
    x_first: f64,
    y_first: f64,
    x_last: f64,
    y_last: f64,
    y_min: f64,
    y_max: f64,
    count: usize,
}

impl Default for Bin4 {
    fn default() -> Self {
        Self {
            x_first: 0.0,
            y_first: 0.0,
            x_last: 0.0,
            y_last: 0.0,
            y_min: f64::MAX,
            y_max: f64::MIN,
            count: 0,
        }
    }
}

// ─── Binary Index I/O ────────────────────────────────────────────────

const INDEX_MAGIC: &[u8; 8] = b"OSCVIDX\0";
const INDEX_VERSION: u32 = 2;

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

    let mut entries = Vec::with_capacity(n_chunks);
    for _ in 0..n_chunks {
        entries.push(ChunkEntry {
            index: entries.len() as u32,
            t_min: read_f64(&mut f)?,
            t_max: read_f64(&mut f)?,
            row_count: read_u64(&mut f)?,
            file_size: { let _ = read_u64(&mut f)?; read_u64(&mut f)? },
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

    /// Main display query: get points for a channel in a time range.
    /// Returns either exact points or M4-downsampled points.
    pub fn get_channel_points(
        &mut self,
        ch_idx: usize,
        delay: f64,
        vis_x_min: f64,
        vis_x_max: f64,
        max_points: usize,
    ) -> Vec<[f64; 2]> {
        let col_idx = ch_idx + 1; // +1 because column 0 is time
        if col_idx >= self.columns.len() || vis_x_min >= vis_x_max {
            return Vec::new();
        }

        let (start, end) = self.chunk_range(vis_x_min, vis_x_max);
        if start >= end {
            return Vec::new();
        }

        let estimated_n: usize = self.entries[start..end]
            .iter()
            .map(|e| e.row_count as usize)
            .sum();

        if estimated_n <= max_points {
            self.fetch_exact(col_idx, delay, vis_x_min, vis_x_max, start, end)
        } else {
            self.m4_downsample(col_idx, delay, vis_x_min, vis_x_max, max_points, start, end)
        }
    }

    /// Fetch raw points (sorted by time) for measurements, FFT, XY, etc.
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

        let mut points: Vec<[f64; 2]> = Vec::new();
        for chunk_idx in start..end {
            if self.ensure_loaded(chunk_idx as u32).is_err() { continue; }
            let chunk = &self.cache[&(chunk_idx as u32)];
            let times = &chunk.columns[0];
            let values = &chunk.columns[col_idx];

            for i in 0..times.len() {
                let t = times[i];
                if t >= vis_x_min && t <= vis_x_max {
                    points.push([t, values[i]]);
                }
            }
        }

        if points.len() > max_points {
            let step = (points.len() / max_points).max(1);
            points = points.into_iter().step_by(step).collect();
        }

        points
    }

    /// Compute min/max/mean/RMS/count for a channel in a time range.
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

        let mut vmin = f64::INFINITY;
        let mut vmax = f64::NEG_INFINITY;
        let mut sum = 0.0f64;
        let mut sum_sq = 0.0f64;
        let mut count = 0usize;

        for chunk_idx in start..end {
            if self.ensure_loaded(chunk_idx as u32).is_err() { continue; }
            let chunk = &self.cache[&(chunk_idx as u32)];
            let times = &chunk.columns[0];
            let values = &chunk.columns[col_idx];

            for i in 0..times.len() {
                let t = times[i];
                if t < vis_x_min || t > vis_x_max { continue; }
                let v = values[i];
                if v < vmin { vmin = v; }
                if v > vmax { vmax = v; }
                sum += v;
                sum_sq += v * v;
                count += 1;
            }
        }

        if count < 2 { return None; }
        let mean = sum / count as f64;
        let rms = (sum_sq / count as f64).sqrt();
        Some((vmin, vmax, mean, rms, count))
    }

    // ─── Internal Helpers ───────────────────────────────────────────

    fn fetch_exact(
        &mut self,
        col_idx: usize,
        delay: f64,
        vis_x_min: f64,
        vis_x_max: f64,
        start_chunk: usize,
        end_chunk: usize,
    ) -> Vec<[f64; 2]> {
        let mut points = Vec::new();
        for chunk_idx in start_chunk..end_chunk {
            if self.ensure_loaded(chunk_idx as u32).is_err() { continue; }
            let chunk = &self.cache[&(chunk_idx as u32)];
            let times = &chunk.columns[0];
            let values = &chunk.columns[col_idx];

            for i in 0..times.len() {
                let t = times[i];
                if t >= vis_x_min && t <= vis_x_max {
                    points.push([t + delay, values[i]]);
                }
            }
        }
        points
    }

    fn m4_downsample(
        &mut self,
        col_idx: usize,
        delay: f64,
        vis_x_min: f64,
        vis_x_max: f64,
        max_points: usize,
        start_chunk: usize,
        end_chunk: usize,
    ) -> Vec<[f64; 2]> {
        let n_bins = (max_points / 4).max(1);
        let range = vis_x_max - vis_x_min;
        if range <= 0.0 { return Vec::new(); }
        let bin_width = range / n_bins as f64;

        let mut bins: Vec<Bin4> = vec![Bin4::default(); n_bins];

        for chunk_idx in start_chunk..end_chunk {
            if self.ensure_loaded(chunk_idx as u32).is_err() { continue; }
            let chunk = &self.cache[&(chunk_idx as u32)];
            let times = &chunk.columns[0];
            let values = &chunk.columns[col_idx];

            for i in 0..times.len() {
                let t = times[i];
                if t < vis_x_min || t > vis_x_max { continue; }
                let v = values[i];
                let bin_idx = (((t - vis_x_min) / bin_width).floor() as usize).min(n_bins - 1);
                let b = &mut bins[bin_idx];
                if b.count == 0 {
                    b.x_first = t;
                    b.y_first = v;
                }
                b.x_last = t;
                b.y_last = v;
                if v < b.y_min { b.y_min = v; }
                if v > b.y_max { b.y_max = v; }
                b.count += 1;
            }
        }

        let mut out = Vec::with_capacity(n_bins * 4);
        for b in &bins {
            if b.count == 0 { continue; }
            out.push([b.x_first + delay, b.y_first]);
            if (b.y_max - b.y_min).abs() > f64::EPSILON {
                if b.y_min < b.y_max {
                    out.push([b.x_first + delay, b.y_min]);
                    out.push([b.x_first + delay, b.y_max]);
                } else {
                    out.push([b.x_first + delay, b.y_max]);
                    out.push([b.x_first + delay, b.y_min]);
                }
            }
            out.push([b.x_last + delay, b.y_last]);
        }
        out
    }
}
