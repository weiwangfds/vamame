//! CSV → TSZ cache layer.
//!
//! On first open, the CSV is converted to Gorilla-compressed TSZ files
//! stored next to the original (`<name>.oscv/<stem>/chunks/chunk_NNNNNN/chM.tsz`).
//!
//! Conversion strategy (direct pipeline, no dependencies):
//! 1. Read CSV with the `csv` crate (fast, zero-copy)
//! 2. Parse each field directly to f64 with whitespace trimming
//! 3. Accumulate rows in chunks of ROWS_PER_GROUP
//! 4. For each chunk, encode each channel as a Gorilla (timestamp, value) stream
//! 5. Track min/max of time column during streaming (no second pass)

use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Name of the cache directory placed next to the source CSV.
const CACHE_DIR: &str = ".oscv";

/// Rows per TSZ chunk.
pub const ROWS_PER_GROUP: usize = 100_000;

// ─── Metadata ────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize)]
pub struct CacheMeta {
    pub md5: String,
    pub n_rows: usize,
    pub n_cols: usize,
    pub columns: Vec<String>,
    pub x_min: f64,
    pub x_max: f64,
    #[serde(default)]
    pub chunked: bool,
    #[serde(default)]
    pub n_chunks: u32,
    #[serde(default)]
    pub rows_per_chunk: u64,
    /// Cache format: "parquet" (legacy) or "tsz" (Gorilla).
    #[serde(default = "default_format")]
    pub format: String,
}

fn default_format() -> String {
    "parquet".to_owned()
}

impl CacheMeta {
    pub fn n_channels(&self) -> usize {
        if self.n_cols > 1 { self.n_cols - 1 } else { 1 }
    }
}

// ─── Path helpers ────────────────────────────────────────────────────

/// Per-file cache directory: `<parent>/.oscv/<file_stem>/`
pub fn cache_dir(csv_path: &Path) -> PathBuf {
    let stem = csv_path
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy();
    csv_path
        .parent()
        .unwrap_or(csv_path)
        .join(CACHE_DIR)
        .join(stem.as_ref())
}

pub fn meta_path(csv_path: &Path) -> PathBuf {
    cache_dir(csv_path).join("metadata.json")
}

pub fn chunks_dir(csv_path: &Path) -> PathBuf {
    cache_dir(csv_path).join("chunks")
}

/// Path to a specific chunk directory containing .tsz files.
pub fn chunk_dir(csv_path: &Path, chunk_idx: u32) -> PathBuf {
    chunks_dir(csv_path).join(format!("chunk_{:06}", chunk_idx))
}

pub fn index_path(csv_path: &Path) -> PathBuf {
    cache_dir(csv_path).join("index.bin")
}

// ─── Fast fingerprint ────────────────────────────────────────────────

/// MD5 of first 32 MB + last 32 MB + file size.
pub fn fingerprint(path: &Path) -> Result<String, String> {
    use md5::{Digest, Md5};
    use std::io::Read;
    let meta = std::fs::metadata(path).map_err(|e| e.to_string())?;
    let size = meta.len();
    let mut hasher = Md5::new();
    let mut f = std::fs::File::open(path).map_err(|e| e.to_string())?;

    let head = 32 * 1024 * 1024;
    let mut buf = vec![0u8; head.min(size as usize)];
    f.read_exact(&mut buf).map_err(|e| e.to_string())?;
    hasher.update(&buf);

    if size > head as u64 * 2 {
        let tail = 32 * 1024 * 1024;
        use std::io::Seek;
        f.seek(std::io::SeekFrom::End(-(tail as i64)))
            .map_err(|e| e.to_string())?;
        buf.resize(tail, 0);
        f.read_exact(&mut buf).map_err(|e| e.to_string())?;
        hasher.update(&buf);
    }
    hasher.update(&size.to_le_bytes());
    Ok(format!("{:x}", hasher.finalize()))
}

// ─── Cache check ─────────────────────────────────────────────────────

pub fn load_meta(csv_path: &Path) -> Option<CacheMeta> {
    let mp = meta_path(csv_path);
    let json = std::fs::read_to_string(&mp).ok()?;
    let meta: CacheMeta = serde_json::from_str(&json).ok()?;
    let fp = fingerprint(csv_path).ok()?;
    if fp != meta.md5 {
        return None;
    }
    if meta.chunked {
        if !index_path(csv_path).exists() || !chunks_dir(csv_path).exists() {
            return None;
        }
        // Validate the index is readable (its version must match the code's
        // INDEX_VERSION). A format bump invalidates the whole cache so the CSV
        // is re-converted with the new layout.
        if super::chunk_store::load_index(&index_path(csv_path)).is_err() {
            return None;
        }
    }
    Some(meta)
}

// ─── TSZ Conversion ──────────────────────────────────────────────────

/// Files larger than this are converted with the parallel mmap path.
/// Smaller files use the streaming `csv`-crate path (well-tested, low overhead).
const PARALLEL_THRESHOLD: u64 = 4 * 1024 * 1024;

/// Aggregate result of converting the whole CSV.
struct Conversion {
    n_rows: usize,
    x_min: f64,
    x_max: f64,
    entries: Vec<super::chunk_store::ChunkEntry>,
    skipped: usize,
}

/// Convert a CSV file to chunked TSZ files with a binary index.
///
/// Each chunk is a directory containing one `.tsz` file per channel.
/// Each `.tsz` file is a Gorilla-compressed (timestamp, value) stream.
///
/// Large files (≥ `PARALLEL_THRESHOLD`) are memory-mapped and parsed in
/// parallel across cores; small files stream through the `csv` crate.
pub fn convert_csv_to_tsz(
    csv_path: &Path,
    progress: &dyn Fn(usize, u64, u64),
) -> Result<CacheMeta, String> {
    let total_size = std::fs::metadata(csv_path)
        .map_err(|e| format!("Cannot stat {}: {e}", csv_path.display()))?
        .len();

    let dir = cache_dir(csv_path);
    // Start from a clean cache dir: a prior run may hold a different index
    // version or stale chunk files, which would corrupt the new layout.
    if dir.exists() {
        let _ = std::fs::remove_dir_all(&dir);
    }
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("Cannot create {}: {e}", dir.display()))?;

    let cdir = chunks_dir(csv_path);
    std::fs::create_dir_all(&cdir)
        .map_err(|e| format!("Cannot create {}: {e}", cdir.display()))?;

    progress(0, 0, total_size);

    // Step 1: detect column count from first line
    let n_cols = count_csv_columns(csv_path)?;
    let col_names: Vec<String> = (0..n_cols).map(|i| format!("column_{}", i + 1)).collect();

    // Compute the file fingerprint up front (it only reads the original CSV's
    // head/tail, independent of conversion). Doing it here lets it overlap with
    // the conversion's own file reads and removes it from the post-100% tail.
    let md5 = fingerprint(csv_path)?;

    // Step 2: convert — parallel (large files) or streaming (small files)
    let conv = if total_size >= PARALLEL_THRESHOLD {
        convert_parallel(csv_path, n_cols, total_size, progress)?
    } else {
        convert_serial(csv_path, n_cols, total_size, progress)?
    };

    // Conversion body is done. Don't jump straight to 100% — the index write,
    // fingerprint (if not yet done) and metadata write still follow, and a
    // stalled 100% bar looks like a hang. Advance to ~97% here.
    let near_done = (total_size * 97 / 100).max(1);
    progress(conv.n_rows, near_done, total_size);

    // Step 3: save metadata + binary index
    if conv.n_rows == 0 {
        return Err("CSV file has no data rows".to_owned());
    }
    let (x_min, x_max) = conv.x_min_filtered();

    let n_chunks = conv.entries.len() as u32;

    super::chunk_store::write_index(
        &index_path(csv_path),
        &conv.entries,
        &col_names,
        conv.n_rows,
        x_min,
        x_max,
        ROWS_PER_GROUP as u64,
    )?;
    // Index written — advance to ~99%.
    progress(conv.n_rows, (total_size * 99 / 100).max(near_done), total_size);

    let meta = CacheMeta {
        md5,
        n_rows: conv.n_rows,
        n_cols,
        columns: col_names,
        x_min,
        x_max,
        chunked: true,
        n_chunks,
        rows_per_chunk: ROWS_PER_GROUP as u64,
        format: "tsz".to_owned(),
    };
    let json = serde_json::to_string_pretty(&meta).map_err(|e| format!("JSON error: {e}"))?;
    std::fs::write(meta_path(csv_path), json)
        .map_err(|e| format!("Cannot write metadata: {e}"))?;

    // All finalization done — now report 100%.
    progress(conv.n_rows, total_size, total_size);


    if conv.skipped > 0 {
        eprintln!("Warning: skipped {} malformed rows", conv.skipped);
    }
    eprintln!(
        "Converted: {} rows, {} cols, {} chunks, range [{:.6e}, {:.6e}]",
        conv.n_rows, n_cols, n_chunks, x_min, x_max
    );

    Ok(meta)
}

impl Conversion {
    /// Return (x_min, x_max) with infinities replaced by 0.0 (degenerate case).
    fn x_min_filtered(&self) -> (f64, f64) {
        let x_min = if self.x_min.is_finite() { self.x_min } else { 0.0 };
        let x_max = if self.x_max.is_finite() { self.x_max } else { 0.0 };
        (x_min, x_max)
    }
}

/// Flush accumulated columns as TSZ-encoded files in a chunk directory.
///
/// Also computes per-channel aggregate statistics (min/max/sum/sum_sq/count,
/// excluding NaN) from the in-memory columns before they are cleared, so that
/// `compute_channel_stats` can later sum these without decoding.
///
/// Returns `(total encoded size, per-channel stats)`.
fn flush_chunk_tsz(
    csv_path: &Path,
    chunk_idx: u32,
    columns: &mut [Vec<f64>],
    n_cols: usize,
) -> Result<(u64, Vec<super::chunk_store::ChannelStats>), String> {
    let cdir = chunk_dir(csv_path, chunk_idx);
    std::fs::create_dir_all(&cdir)
        .map_err(|e| format!("Cannot create chunk dir {}: {e}", cdir.display()))?;

    // Column 0 is time. Encode each data channel (1..n_cols) as (time, value),
    // and collect its precomputed stats in the same pass.
    let timestamps: &[f64] = &columns[0];
    let mut total_size = 0u64;
    let mut stats = Vec::with_capacity(n_cols.saturating_sub(1));
    for ch in 1..n_cols {
        let encoded = super::tsz_codec::encode_channel(timestamps, &columns[ch]);
        let path = cdir.join(format!("ch{}.tsz", ch - 1));
        std::fs::write(&path, &encoded)
            .map_err(|e| format!("Cannot write {}: {e}", path.display()))?;
        total_size += encoded.len() as u64;
        stats.push(super::chunk_store::ChannelStats::from_values(&columns[ch]));
    }

    // Clear buffers for next chunk
    for col in columns.iter_mut() {
        col.clear();
    }

    Ok((total_size, stats))
}

// ─── Serial conversion (small files) ─────────────────────────────────

/// Streaming conversion via the `csv` crate. Used for files below
/// `PARALLEL_THRESHOLD`; mature path that tolerates quoting/escaping.
fn convert_serial(
    csv_path: &Path,
    n_cols: usize,
    total_size: u64,
    progress: &dyn Fn(usize, u64, u64),
) -> Result<Conversion, String> {
    let file = std::fs::File::open(csv_path)
        .map_err(|e| format!("Cannot open {}: {e}", csv_path.display()))?;
    let buf_reader = std::io::BufReader::with_capacity(4 * 1024 * 1024, file);
    let (mut counting_reader, bytes_counter) = CountingReader::new(buf_reader);

    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(false)
        .trim(csv::Trim::None)
        .from_reader(&mut counting_reader);

    let mut columns: Vec<Vec<f64>> =
        (0..n_cols).map(|_| Vec::with_capacity(ROWS_PER_GROUP)).collect();
    let mut n_rows: usize = 0;
    let mut x_min = f64::INFINITY;
    let mut x_max = f64::NEG_INFINITY;
    let mut chunk_entries: Vec<super::chunk_store::ChunkEntry> = Vec::new();
    let mut chunk_t_min = f64::INFINITY;
    let mut chunk_t_max = f64::NEG_INFINITY;
    let mut current_chunk_rows: u64 = 0;
    let mut skipped: usize = 0;

    for result in rdr.byte_records() {
        let record = match result {
            Ok(r) => r,
            Err(_) => { skipped += 1; continue; }
        };
        if record.len() != n_cols {
            skipped += 1;
            continue;
        }

        for (col_idx, field) in record.iter().enumerate() {
            if col_idx >= n_cols { break; }
            let val = if field.is_empty() {
                f64::NAN
            } else {
                let s = unsafe { std::str::from_utf8_unchecked(field) };
                fast_parse_f64(s)
            };
            columns[col_idx].push(val);
        }

        let t = *columns[0].last().unwrap();
        if t.is_finite() {
            if t < chunk_t_min { chunk_t_min = t; }
            if t > chunk_t_max { chunk_t_max = t; }
            if t < x_min { x_min = t; }
            if t > x_max { x_max = t; }
        }
        current_chunk_rows += 1;
        n_rows += 1;

        if columns[0].len() >= ROWS_PER_GROUP {
            let idx = chunk_entries.len();
            let (file_size, stats) = flush_chunk_tsz(csv_path, idx as u32, &mut columns, n_cols)?;
            chunk_entries.push(super::chunk_store::ChunkEntry {
                index: idx as u32,
                t_min: chunk_t_min,
                t_max: chunk_t_max,
                row_count: current_chunk_rows,
                file_size,
                stats,
            });
            chunk_t_min = f64::INFINITY;
            chunk_t_max = f64::NEG_INFINITY;
            current_chunk_rows = 0;
            progress(n_rows, bytes_counter.get(), total_size);
        }
    }

    if !columns[0].is_empty() {
        let idx = chunk_entries.len();
        let (file_size, stats) = flush_chunk_tsz(csv_path, idx as u32, &mut columns, n_cols)?;
        chunk_entries.push(super::chunk_store::ChunkEntry {
            index: idx as u32,
            t_min: chunk_t_min,
            t_max: chunk_t_max,
            row_count: current_chunk_rows,
            file_size,
            stats,
        });
    }

    Ok(Conversion { n_rows, x_min, x_max, entries: chunk_entries, skipped })
}

// ─── Parallel conversion (large files) ───────────────────────────────

/// Result of parsing one byte segment of the file.
struct SegmentResult {
    entries: Vec<super::chunk_store::ChunkEntry>,
    n_rows: usize,
    x_min: f64,
    x_max: f64,
    skipped: usize,
}

/// Parallel mmap-based conversion.
///
/// The file is memory-mapped and split into newline-aligned byte segments.
/// Pass 1 counts rows per segment (to assign global chunk indices); pass 2
/// parses + encodes each segment in parallel. Parsing runs on a worker thread
/// (so it can use the rayon pool) while this thread polls progress atomics
/// and reports them via the callback — keeping the non-`Send` callback on one
/// thread.
fn convert_parallel(
    csv_path: &Path,
    n_cols: usize,
    total_size: u64,
    progress: &dyn Fn(usize, u64, u64),
) -> Result<Conversion, String> {
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::sync::Arc;

    let rows_atomic = Arc::new(AtomicUsize::new(0));
    let bytes_atomic = Arc::new(AtomicU64::new(0));
    let path_buf = csv_path.to_path_buf();
    let rows_for_thread = rows_atomic.clone();
    let bytes_for_thread = bytes_atomic.clone();

    // Worker thread owns the mmap and drives both passes via the rayon pool.
    let worker = std::thread::spawn(move || -> Result<Conversion, String> {
        let file = std::fs::File::open(&path_buf)
            .map_err(|e| format!("Cannot open {}: {e}", path_buf.display()))?;
        let mmap = unsafe { memmap2::MmapOptions::new().map(&file) }
            .map_err(|e| format!("Cannot mmap {}: {e}", path_buf.display()))?;
        let data: &[u8] = &mmap;
        let total_bytes = data.len();

        // ── Build newline-aligned segment start offsets ──
        // Aim for ~4 segments per core for good load balancing via rayon's
        // work-stealing, but keep each segment ≥ 16 MB so it spans at least one
        // ~100k-row chunk (avoiding an explosion of tiny partial chunks).
        let n_cores = rayon::current_num_threads().max(1);
        let min_seg = 16 * 1024 * 1024;
        let raw_seg_len = (total_bytes / (n_cores * 4)).max(min_seg).max(1);

        let mut starts: Vec<usize> = vec![0];
        let mut cursor = raw_seg_len;
        while cursor < total_bytes {
            match data[cursor..].iter().position(|&b| b == b'\n') {
                Some(off) => {
                    let start = cursor + off + 1;
                    if start >= total_bytes { break; }
                    starts.push(start);
                    cursor = start.saturating_add(raw_seg_len);
                }
                None => break,
            }
        }
        let n_segs = starts.len();

        let seg_range = |i: usize| -> (usize, usize) {
            let st = starts[i];
            let en = if i + 1 < n_segs { starts[i + 1] } else { total_bytes };
            (st, en)
        };

        // ── Pass 1: count rows per segment in parallel ──
        let row_counts: Vec<usize> = (0..n_segs)
            .into_par_iter()
            .map(|i| {
                let (s, e) = seg_range(i);
                bytecount::count(&data[s..e], b'\n')
            })
            .collect();

        let total_rows: usize = row_counts.iter().sum();
        let avg_row_bytes = if total_rows > 0 {
            total_bytes as f64 / total_rows as f64
        } else {
            1.0
        };

        // Per-segment chunk index base (cumulative ceil(rows / ROWS_PER_GROUP)).
        let mut bases = Vec::with_capacity(n_segs);
        let mut acc: u32 = 0;
        for &rc in &row_counts {
            bases.push(acc);
            let chunks = ((rc + ROWS_PER_GROUP - 1) / ROWS_PER_GROUP) as u32;
            acc = acc.saturating_add(chunks);
        }

        // ── Pass 2: parse + encode each segment in parallel ──
        let results: Vec<SegmentResult> = (0..n_segs)
            .into_par_iter()
            .map(|i| {
                let (s, e) = seg_range(i);
                parse_segment(
                    &data[s..e],
                    n_cols,
                    bases[i],
                    &path_buf,
                    &rows_for_thread,
                    &bytes_for_thread,
                    avg_row_bytes,
                )
            })
            .collect();

        // ── Merge in segment order (already time-ordered) ──
        let mut entries = Vec::new();
        let mut n_rows = 0usize;
        let mut x_min = f64::INFINITY;
        let mut x_max = f64::NEG_INFINITY;
        let mut skipped = 0usize;
        for r in results {
            entries.extend(r.entries);
            n_rows += r.n_rows;
            skipped += r.skipped;
            if r.x_min < x_min { x_min = r.x_min; }
            if r.x_max > x_max { x_max = r.x_max; }
        }

        Ok(Conversion { n_rows, x_min, x_max, entries, skipped })
    });

    // Poll progress on this thread until the worker finishes.
    while !worker.is_finished() {
        let rows = rows_atomic.load(Ordering::Relaxed);
        let bytes = bytes_atomic.load(Ordering::Relaxed);
        progress(rows, bytes, total_size);
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    progress(
        rows_atomic.load(Ordering::Relaxed),
        total_size,
        total_size,
    );

    worker
        .join()
        .unwrap_or_else(|_| Err("conversion worker panicked".to_owned()))
}

/// Parse one byte segment into TSZ chunk files.
///
/// Rows are accumulated into `ROWS_PER_GROUP`-sized column buffers and flushed
/// to chunk directories indexed from `base_chunk_idx`. Progress atomics are
/// updated as chunks are written.
fn parse_segment(
    data: &[u8],
    n_cols: usize,
    base_chunk_idx: u32,
    csv_path: &Path,
    rows_atomic: &std::sync::atomic::AtomicUsize,
    bytes_atomic: &std::sync::atomic::AtomicU64,
    avg_row_bytes: f64,
) -> SegmentResult {
    use std::sync::atomic::Ordering;

    let mut columns: Vec<Vec<f64>> =
        (0..n_cols).map(|_| Vec::with_capacity(ROWS_PER_GROUP)).collect();
    let mut entries: Vec<super::chunk_store::ChunkEntry> = Vec::new();
    let mut n_rows: usize = 0;
    let mut x_min = f64::INFINITY;
    let mut x_max = f64::NEG_INFINITY;
    let mut chunk_t_min = f64::INFINITY;
    let mut chunk_t_max = f64::NEG_INFINITY;
    let mut current_chunk_rows: u64 = 0;
    let mut local_chunk: u32 = 0;
    let mut skipped: usize = 0;
    // Rows already reported to the shared progress counters. We `fetch_add`
    // only the delta so that concurrent segments accumulate correctly.
    let mut reported_rows: usize = 0;

    for line in data.split(|&b| b == b'\n') {
        // strip trailing CR (CRLF line endings)
        let line = if line.last() == Some(&b'\r') { &line[..line.len() - 1] } else { line };
        if line.is_empty() { continue; }

        // Split on commas and parse each field directly into the column buffers.
        let mut col_idx = 0usize;
        let mut too_many = false;
        for field in line.split(|&b| b == b',') {
            if col_idx >= n_cols { too_many = true; break; }
            let val = parse_field(field);
            columns[col_idx].push(val);
            col_idx += 1;
        }

        if col_idx != n_cols || too_many {
            // Malformed row — undo the partial pushes so buffers stay aligned.
            for c in 0..col_idx {
                columns[c].pop();
            }
            skipped += 1;
            continue;
        }

        let t = columns[0][columns[0].len() - 1];
        if t.is_finite() {
            if t < chunk_t_min { chunk_t_min = t; }
            if t > chunk_t_max { chunk_t_max = t; }
            if t < x_min { x_min = t; }
            if t > x_max { x_max = t; }
        }
        current_chunk_rows += 1;
        n_rows += 1;

        if columns[0].len() >= ROWS_PER_GROUP {
            let global_idx = base_chunk_idx + local_chunk;
            let (file_size, stats) = flush_chunk_tsz(csv_path, global_idx, &mut columns, n_cols)
                .unwrap_or((0, Vec::new()));
            entries.push(super::chunk_store::ChunkEntry {
                index: global_idx,
                t_min: chunk_t_min,
                t_max: chunk_t_max,
                row_count: current_chunk_rows,
                file_size,
                stats,
            });
            chunk_t_min = f64::INFINITY;
            chunk_t_max = f64::NEG_INFINITY;
            current_chunk_rows = 0;
            local_chunk += 1;

            // Report progress as a delta so concurrent segments accumulate.
            let delta = n_rows - reported_rows;
            if delta > 0 {
                rows_atomic.fetch_add(delta, Ordering::Relaxed);
                bytes_atomic.fetch_add((delta as f64 * avg_row_bytes) as u64, Ordering::Relaxed);
                reported_rows = n_rows;
            }
        }
    }

    // Flush remaining rows of this segment as a final (possibly partial) chunk.
    if !columns[0].is_empty() {
        let global_idx = base_chunk_idx + local_chunk;
        let (file_size, stats) = flush_chunk_tsz(csv_path, global_idx, &mut columns, n_cols)
            .unwrap_or((0, Vec::new()));
        entries.push(super::chunk_store::ChunkEntry {
            index: global_idx,
            t_min: chunk_t_min,
            t_max: chunk_t_max,
            row_count: current_chunk_rows,
            file_size,
            stats,
        });
    }

    // Account for any rows not yet reported (final partial chunk).
    let delta = n_rows - reported_rows;
    if delta > 0 {
        rows_atomic.fetch_add(delta, Ordering::Relaxed);
    }

    SegmentResult { entries, n_rows, x_min, x_max, skipped }
}

/// Parse a numeric CSV field (`&[u8]`) into `f64`, returning NaN on failure.
#[inline]
fn parse_field(field: &[u8]) -> f64 {
    let s = match std::str::from_utf8(field) {
        Ok(s) => s.trim(),
        Err(_) => return f64::NAN,
    };
    if s.is_empty() {
        f64::NAN
    } else {
        fast_float::parse(s).unwrap_or(f64::NAN)
    }
}

/// Fast f64 parser that handles common edge cases.
///
/// Uses the `fast-float` crate (Eisel-Lemire / Clinger), which is several
/// times faster than `std::str::parse` for scientific notation. Leading/
/// trailing whitespace is trimmed first (fields may carry a leading space).
#[inline]
fn fast_parse_f64(s: &str) -> f64 {
    fast_float::parse(s.trim()).unwrap_or(f64::NAN)
}

/// Count columns from the first non-empty line of a CSV file.
fn count_csv_columns(path: &Path) -> Result<usize, String> {
    use std::io::BufRead;
    let file = std::fs::File::open(path).map_err(|e| format!("Cannot open: {e}"))?;
    let mut reader = std::io::BufReader::new(file);
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader
            .read_line(&mut line)
            .map_err(|e| format!("Read error: {e}"))?;
        if n == 0 {
            return Err("Empty CSV file".to_owned());
        }
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.split(',').count());
        }
    }
}

/// Wrapper around `Read` that tracks total bytes read via shared counter.
struct CountingReader<R> {
    inner: R,
    bytes_read: std::rc::Rc<std::cell::Cell<u64>>,
}

impl<R> CountingReader<R> {
    fn new(inner: R) -> (Self, std::rc::Rc<std::cell::Cell<u64>>) {
        let counter = std::rc::Rc::new(std::cell::Cell::new(0u64));
        let reader = Self { inner, bytes_read: counter.clone() };
        (reader, counter)
    }
}

impl<R: std::io::Read> std::io::Read for CountingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.bytes_read.set(self.bytes_read.get() + n as u64);
        Ok(n)
    }
}
