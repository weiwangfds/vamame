//! Indexed CSV data layer for files too large to fit in RAM.
//!
//! A single sequential pass builds a small index file (`.csv.idx`) that stores
//! byte offsets and time values at regular intervals.  Subsequent opens load
//! the index (a few MB) instantly, and visible-range reads use `seek()` +
//! binary search to touch only the relevant portion of the file.
//!
//! For a 200 GB / 1.7-billion-row file the index is ~5–15 MB and every
//! zoom/pan frame reads at most a few MB of CSV from disk.

use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

// ── magic + version for the index file ──
const IDX_MAGIC: &[u8; 8] = b"OSCVIDX\0";
const IDX_VERSION: u32 = 1;

/// How many rows to skip between index entries.
/// For a 200 GB file (~1.7 B rows) this yields ~170 K entries ≈ 3 MB index.
const INDEX_STEP: u64 = 10_000;

/// Maximum rows to read in a single seek-based query.
/// Prevents accidentally reading hundreds of millions of rows.
const MAX_ROWS_PER_QUERY: usize = 2_000_000;

// ───────────────────────────────────────────────────────────────────────
// Index data structure
// ───────────────────────────────────────────────────────────────────────

/// On-disk index for fast random access into a CSV file.
///
/// Binary layout (all little-endian):
/// ```text
/// [8 bytes]  magic  "OSCVIDX\0"
/// [4 bytes]  version  (u32)
/// [4 bytes]  n_cols   (u32)
/// [8 bytes]  total_rows (u64)
/// [8 bytes]  index_step (u64)
/// [8 bytes]  x_min    (f64)
/// [8 bytes]  x_max    (f64)
/// [N×16 bytes] entries: (byte_offset: u64, time_value: f64)
/// ```
#[derive(Clone)]
pub(crate) struct CsvIndex {
    pub path: PathBuf,
    pub n_cols: u32,
    pub total_rows: u64,
    pub step: u64,
    pub x_min: f64,
    pub x_max: f64,
    /// (byte_offset, time_value) for every `step`-th row.
    pub entries: Vec<(u64, f64)>,
}

impl CsvIndex {
    /// Index file path: `<csv_path>.idx`
    pub fn idx_path(csv_path: &Path) -> PathBuf {
        let mut p = csv_path.to_path_buf();
        p.set_extension("csv.idx");
        p
    }

    /// Try to load an existing index from disk.
    /// Returns `None` if the file doesn't exist or is stale.
    pub fn load_from_disk(csv_path: &Path) -> Option<Self> {
        let idx_path = Self::idx_path(csv_path);
        if !idx_path.exists() {
            return None;
        }

        // Check that the CSV hasn't been modified since the index was built.
        let csv_meta = std::fs::metadata(csv_path).ok()?;
        let idx_meta = std::fs::metadata(&idx_path).ok()?;
        if idx_meta.modified().ok()? <= csv_meta.modified().ok()? {
            // Index is older than CSV — stale, ignore
            return None;
        }

        let mut f = std::io::BufReader::new(std::fs::File::open(&idx_path).ok()?);

        // Read header
        let mut magic = [0u8; 8];
        f.read_exact(&mut magic).ok()?;
        if &magic != IDX_MAGIC {
            return None;
        }
        let version = read_u32(&mut f).ok()?;
        if version != IDX_VERSION {
            return None;
        }
        let n_cols = read_u32(&mut f).ok()?;
        let total_rows = read_u64(&mut f).ok()?;
        let step = read_u64(&mut f).ok()?;
        let x_min = read_f64(&mut f).ok()?;
        let x_max = read_f64(&mut f).ok()?;

        // Read entries
        let n_entries = (total_rows / step + 1) as usize;
        let mut entries = Vec::with_capacity(n_entries);
        for _ in 0..n_entries {
            let offset = read_u64(&mut f).ok()?;
            let time = read_f64(&mut f).ok()?;
            entries.push((offset, time));
        }

        Some(Self {
            path: csv_path.to_path_buf(),
            n_cols,
            total_rows,
            step,
            x_min,
            x_max,
            entries,
        })
    }

    /// Build the index with a progress callback and save to disk.
    ///
    /// `progress`: called periodically with (rows_scanned, byte_offset, total_bytes).
    pub fn build(
        csv_path: &Path,
        progress: &dyn Fn(u64, u64, u64),
    ) -> Result<Self, String> {
        let file_size = std::fs::metadata(csv_path)
            .map_err(|e| format!("Cannot stat {}: {e}", csv_path.display()))?
            .len();

        let file = std::fs::File::open(csv_path)
            .map_err(|e| format!("Cannot open {}: {e}", csv_path.display()))?;
        let mut reader = BufReader::with_capacity(8 * 1024 * 1024, file);

        let mut entries: Vec<(u64, f64)> = Vec::new();
        let mut row_count: u64 = 0;
        let mut n_cols: u32 = 0;
        let mut x_min: f64 = f64::MAX;
        let mut x_max: f64 = f64::MIN;
        let mut last_progress_byte: u64 = 0;

        let mut line = String::with_capacity(512);
        loop {
            let offset = reader.stream_position().unwrap_or(0);
            line.clear();
            let bytes = reader.read_line(&mut line).map_err(|e| format!("Read error: {e}"))?;
            if bytes == 0 {
                break; // EOF
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            if row_count == 0 {
                // Detect number of columns from first data line
                n_cols = trimmed.split(',').count() as u32;
            }

            // Parse time value (first column)
            if let Some(time_str) = trimmed.split(',').next() {
                if let Ok(t) = time_str.trim().parse::<f64>() {
                    if t < x_min { x_min = t; }
                    if t > x_max { x_max = t; }

                    if row_count % INDEX_STEP == 0 {
                        entries.push((offset, t));
                    }
                }
            }

            row_count += 1;

            // Progress callback every 100 MB
            if offset - last_progress_byte >= 100_000_000 {
                progress(row_count, offset, file_size);
                last_progress_byte = offset;
            }
        }

        if row_count == 0 {
            return Err("File has no data rows".to_owned());
        }

        let idx = Self {
            path: csv_path.to_path_buf(),
            n_cols,
            total_rows: row_count,
            step: INDEX_STEP,
            x_min,
            x_max,
            entries,
        };

        // Save to disk
        idx.save_to_disk()?;

        Ok(idx)
    }

    fn save_to_disk(&self) -> Result<(), String> {
        let idx_path = Self::idx_path(&self.path);
        let mut f = std::io::BufWriter::new(
            std::fs::File::create(&idx_path)
                .map_err(|e| format!("Cannot create {}: {e}", idx_path.display()))?,
        );

        f.write_all(IDX_MAGIC).map_err(|e| format!("Write error: {e}"))?;
        write_u32(&mut f, IDX_VERSION)?;
        write_u32(&mut f, self.n_cols)?;
        write_u64(&mut f, self.total_rows)?;
        write_u64(&mut f, self.step)?;
        write_f64(&mut f, self.x_min)?;
        write_f64(&mut f, self.x_max)?;

        for &(offset, time) in &self.entries {
            write_u64(&mut f, offset)?;
            write_f64(&mut f, time)?;
        }

        Ok(())
    }

    /// Number of data channels (columns minus the time column).
    pub fn n_channels(&self) -> usize {
        if self.n_cols > 1 {
            (self.n_cols - 1) as usize
        } else {
            1
        }
    }

    /// Use binary search on the index to find the approximate byte offset
    /// for a given time value.  Returns the byte offset to start reading.
    pub fn seek_offset_for_time(&self, target_time: f64) -> u64 {
        if self.entries.is_empty() {
            return 0;
        }
        // Binary search for the last entry with time <= target_time
        let mut lo = 0usize;
        let mut hi = self.entries.len();
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.entries[mid].1 <= target_time {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        // lo is the first entry with time > target_time; back up one
        if lo > 0 {
            // Go back one more step to ensure we don't miss any rows
            let start = lo.saturating_sub(1);
            self.entries[start].0
        } else {
            0
        }
    }
}

// ───────────────────────────────────────────────────────────────────────
// Query: read visible range from indexed CSV
// ───────────────────────────────────────────────────────────────────────

/// Read points for a single channel within [vis_x_min, vis_x_max],
/// downsampled to at most `max_points` points using M4 algorithm.
///
/// This function seeks to the approximate start position, reads rows
/// sequentially until past vis_x_max, then applies M4 downsampling.
pub fn query_channel_points(
    idx: &CsvIndex,
    ch_idx: usize,
    delay: f64,
    vis_x_min: f64,
    vis_x_max: f64,
    max_points: usize,
) -> Vec<[f64; 2]> {
    if vis_x_min >= vis_x_max {
        return Vec::new();
    }

    let col_idx = if idx.n_cols > 1 { ch_idx + 1 } else { 0 };
    if col_idx >= idx.n_cols as usize {
        return Vec::new();
    }

    // Seek to approximate start position
    let start_offset = idx.seek_offset_for_time(vis_x_min);

    let file = match std::fs::File::open(&idx.path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("Cannot open {}: {e}", idx.path.display());
            return Vec::new();
        }
    };
    let mut reader = BufReader::with_capacity(4 * 1024 * 1024, file);
    if reader.seek(SeekFrom::Start(start_offset)).is_err() {
        return Vec::new();
    }

    // Read rows within [vis_x_min, vis_x_max], capped at MAX_ROWS_PER_QUERY
    let mut raw_points: Vec<[f64; 2]> = Vec::new();
    let mut line = String::with_capacity(512);
    let mut _rows_read: usize = 0;

    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => break,
        }
        _rows_read += 1;

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Fast parse: split by comma, extract time (col 0) and target channel
        let mut col = 0usize;
        let mut time_val: Option<f64> = None;
        let mut ch_val: Option<f64> = None;

        for field in trimmed.split(',') {
            if col == 0 {
                time_val = field.trim().parse::<f64>().ok();
            } else if col == col_idx {
                ch_val = field.trim().parse::<f64>().ok();
                break; // no need to parse further columns
            }
            col += 1;
        }

        let (Some(t), Some(v)) = (time_val, ch_val) else { continue };

        // If we're past the visible range, stop reading
        if t > vis_x_max {
            break;
        }
        if t < vis_x_min {
            continue;
        }

        raw_points.push([t, v]);

        if raw_points.len() >= MAX_ROWS_PER_QUERY {
            break;
        }
    }

    if raw_points.is_empty() {
        return Vec::new();
    }

    // If raw points fit within max_points, return as-is
    if raw_points.len() <= max_points {
        for p in &mut raw_points {
            p[0] += delay;
        }
        return raw_points;
    }

    // M4 downsample: first/min/max/last per bin
    m4_downsample(&raw_points, delay, vis_x_min, vis_x_max, max_points)
}

/// Read raw (non-downsampled) points for measurement calculations.
pub fn query_raw_points(
    idx: &CsvIndex,
    ch_idx: usize,
    vis_x_min: f64,
    vis_x_max: f64,
    max_points: usize,
) -> Vec<[f64; 2]> {
    if vis_x_min >= vis_x_max {
        return Vec::new();
    }

    let col_idx = if idx.n_cols > 1 { ch_idx + 1 } else { 0 };
    if col_idx >= idx.n_cols as usize {
        return Vec::new();
    }

    let start_offset = idx.seek_offset_for_time(vis_x_min);

    let file = match std::fs::File::open(&idx.path) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };
    let mut reader = BufReader::with_capacity(4 * 1024 * 1024, file);
    if reader.seek(SeekFrom::Start(start_offset)).is_err() {
        return Vec::new();
    }

    let mut points: Vec<[f64; 2]> = Vec::new();
    let mut line = String::with_capacity(512);

    // Calculate subsampling step
    // Estimate total rows in range
    let est_rows = estimate_rows_in_range(idx, vis_x_min, vis_x_max);
    let step = if est_rows > max_points as u64 {
        (est_rows / max_points as u64).max(1) as usize
    } else {
        1
    };

    let mut row_in_range: usize = 0;

    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => break,
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let mut col = 0usize;
        let mut time_val: Option<f64> = None;
        let mut ch_val: Option<f64> = None;

        for field in trimmed.split(',') {
            if col == 0 {
                time_val = field.trim().parse::<f64>().ok();
            } else if col == col_idx {
                ch_val = field.trim().parse::<f64>().ok();
                break;
            }
            col += 1;
        }

        let (Some(t), Some(v)) = (time_val, ch_val) else { continue };

        if t > vis_x_max {
            break;
        }
        if t < vis_x_min {
            continue;
        }

        if row_in_range % step == 0 {
            points.push([t, v]);
        }
        row_in_range += 1;

        if points.len() >= max_points {
            break;
        }
    }

    points
}

/// Estimate how many rows fall within [vis_x_min, vis_x_max] using the index.
fn estimate_rows_in_range(idx: &CsvIndex, vis_x_min: f64, vis_x_max: f64) -> u64 {
    let total_range = idx.x_max - idx.x_min;
    if total_range <= 0.0 {
        return idx.total_rows;
    }
    let vis_range = vis_x_max - vis_x_min;
    let fraction = (vis_range / total_range).clamp(0.0, 1.0);
    ((idx.total_rows as f64) * fraction) as u64
}

// ───────────────────────────────────────────────────────────────────────
// M4 downsample (pure Rust, no Polars dependency)
// ───────────────────────────────────────────────────────────────────────

/// M4 peak-detect downsample: emit first/min/max/last per bin.
fn m4_downsample(
    points: &[[f64; 2]],
    delay: f64,
    vis_x_min: f64,
    vis_x_max: f64,
    max_points: usize,
) -> Vec<[f64; 2]> {
    let n_bins = (max_points / 4).max(1);
    let range = vis_x_max - vis_x_min;
    let bin_width = range / n_bins as f64;

    // Per-bin accumulators: (x_first, y_first, x_last, y_last, y_min, y_max)
    let mut bins: Vec<Bin4> = vec![Bin4::default(); n_bins];

    for &[x, y] in points {
        let bin_idx = (((x - vis_x_min) / bin_width).floor() as usize).min(n_bins - 1);
        let b = &mut bins[bin_idx];
        if b.count == 0 {
            b.x_first = x;
            b.y_first = y;
        }
        b.x_last = x;
        b.y_last = y;
        if y < b.y_min { b.y_min = y; }
        if y > b.y_max { b.y_max = y; }
        b.count += 1;
    }

    let mut out = Vec::with_capacity(n_bins * 4);
    for b in &bins {
        if b.count == 0 {
            continue;
        }
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

// ───────────────────────────────────────────────────────────────────────
// Binary I/O helpers
// ───────────────────────────────────────────────────────────────────────

fn read_u32<R: Read>(r: &mut R) -> Result<u32, String> {
    let mut buf = [0u8; 4];
    r.read_exact(&mut buf).map_err(|e| format!("IO error: {e}"))?;
    Ok(u32::from_le_bytes(buf))
}

fn read_u64<R: Read>(r: &mut R) -> Result<u64, String> {
    let mut buf = [0u8; 8];
    r.read_exact(&mut buf).map_err(|e| format!("IO error: {e}"))?;
    Ok(u64::from_le_bytes(buf))
}

fn read_f64<R: Read>(r: &mut R) -> Result<f64, String> {
    let mut buf = [0u8; 8];
    r.read_exact(&mut buf).map_err(|e| format!("IO error: {e}"))?;
    Ok(f64::from_le_bytes(buf))
}

fn write_u32<W: std::io::Write>(w: &mut W, v: u32) -> Result<(), String> {
    w.write_all(&v.to_le_bytes()).map_err(|e| format!("Write error: {e}"))
}

fn write_u64<W: std::io::Write>(w: &mut W, v: u64) -> Result<(), String> {
    w.write_all(&v.to_le_bytes()).map_err(|e| format!("Write error: {e}"))
}

fn write_f64<W: std::io::Write>(w: &mut W, v: f64) -> Result<(), String> {
    w.write_all(&v.to_le_bytes()).map_err(|e| format!("Write error: {e}"))
}

use std::io::Write;
