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

/// Path to a channel's .tsz file within a chunk directory.
pub fn tsz_channel_path(csv_path: &Path, chunk_idx: u32, channel_idx: usize) -> PathBuf {
    chunk_dir(csv_path, chunk_idx).join(format!("ch{}.tsz", channel_idx))
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
    }
    Some(meta)
}

// ─── TSZ Conversion ──────────────────────────────────────────────────

/// Convert a CSV file to chunked TSZ files with a binary index.
///
/// Each chunk is a directory containing one `.tsz` file per channel.
/// Each `.tsz` file is a Gorilla-compressed (timestamp, value) stream.
pub fn convert_csv_to_tsz(
    csv_path: &Path,
    progress: &dyn Fn(usize, u64, u64),
) -> Result<CacheMeta, String> {
    let total_size = std::fs::metadata(csv_path)
        .map_err(|e| format!("Cannot stat {}: {e}", csv_path.display()))?
        .len();

    let dir = cache_dir(csv_path);
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("Cannot create {}: {e}", dir.display()))?;

    let cdir = chunks_dir(csv_path);
    std::fs::create_dir_all(&cdir)
        .map_err(|e| format!("Cannot create {}: {e}", cdir.display()))?;

    progress(0, 0, total_size);

    // Step 1: detect column count from first line
    let n_cols = count_csv_columns(csv_path)?;
    let col_names: Vec<String> = (0..n_cols).map(|i| format!("column_{}", i + 1)).collect();

    // Step 2: open CSV reader with whitespace trimming
    let file = std::fs::File::open(csv_path)
        .map_err(|e| format!("Cannot open {}: {e}", csv_path.display()))?;
    let buf_reader = std::io::BufReader::with_capacity(4 * 1024 * 1024, file);
    let (mut counting_reader, bytes_counter) = CountingReader::new(buf_reader);

    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(false)
        .trim(csv::Trim::All)
        .from_reader(&mut counting_reader);

    // Step 3: streaming conversion to chunked TSZ
    let mut columns: Vec<Vec<f64>> = (0..n_cols).map(|_| Vec::with_capacity(ROWS_PER_GROUP)).collect();
    let mut n_rows: usize = 0;
    let mut x_min = f64::INFINITY;
    let mut x_max = f64::NEG_INFINITY;

    let mut chunk_entries: Vec<super::chunk_store::ChunkEntry> = Vec::new();
    let mut chunk_t_min = f64::INFINITY;
    let mut chunk_t_max = f64::NEG_INFINITY;
    let mut current_chunk_rows: u64 = 0;
    let mut skipped: usize = 0;

    let record_iter = rdr.byte_records();
    for result in record_iter {
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

        // Flush when buffer is full
        if columns[0].len() >= ROWS_PER_GROUP {
            let idx = chunk_entries.len();
            flush_chunk_tsz(csv_path, idx as u32, &mut columns, n_cols)?;

            chunk_entries.push(super::chunk_store::ChunkEntry {
                index: idx as u32,
                t_min: chunk_t_min,
                t_max: chunk_t_max,
                row_count: current_chunk_rows,
                file_size: chunk_tsz_size(csv_path, idx as u32, n_cols),
            });

            chunk_t_min = f64::INFINITY;
            chunk_t_max = f64::NEG_INFINITY;
            current_chunk_rows = 0;

            progress(n_rows, bytes_counter.get(), total_size);
        }
    }

    // Flush remaining rows
    if !columns[0].is_empty() {
        let idx = chunk_entries.len();
        flush_chunk_tsz(csv_path, idx as u32, &mut columns, n_cols)?;

        chunk_entries.push(super::chunk_store::ChunkEntry {
            index: idx as u32,
            t_min: chunk_t_min,
            t_max: chunk_t_max,
            row_count: current_chunk_rows,
            file_size: chunk_tsz_size(csv_path, idx as u32, n_cols),
        });
    }

    progress(n_rows, total_size, total_size);

    // Step 4: save metadata + binary index
    if n_rows == 0 {
        return Err("CSV file has no data rows".to_owned());
    }
    if !x_min.is_finite() { x_min = 0.0; }
    if !x_max.is_finite() { x_max = 0.0; }

    let n_chunks = chunk_entries.len() as u32;

    super::chunk_store::write_index(
        &index_path(csv_path),
        &chunk_entries,
        &col_names,
        n_rows,
        x_min,
        x_max,
        ROWS_PER_GROUP as u64,
    )?;

    let md5 = fingerprint(csv_path)?;
    let meta = CacheMeta {
        md5,
        n_rows,
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

    if skipped > 0 {
        eprintln!("Warning: skipped {skipped} malformed rows");
    }
    eprintln!(
        "Converted: {} rows, {} cols, {} chunks, range [{:.6e}, {:.6e}]",
        n_rows, n_cols, n_chunks, x_min, x_max
    );

    Ok(meta)
}

/// Flush accumulated columns as TSZ-encoded files in a chunk directory.
fn flush_chunk_tsz(
    csv_path: &Path,
    chunk_idx: u32,
    columns: &mut [Vec<f64>],
    n_cols: usize,
) -> Result<(), String> {
    let cdir = chunk_dir(csv_path, chunk_idx);
    std::fs::create_dir_all(&cdir)
        .map_err(|e| format!("Cannot create chunk dir {}: {e}", cdir.display()))?;

    // Column 0 is time. For each data channel (1..n_cols), encode (time, value) pairs.
    let timestamps = &columns[0];
    for ch in 0..n_cols {
        if ch == 0 { continue; } // skip time column itself
        let encoded = super::tsz_codec::encode_channel(timestamps, &columns[ch]);
        let path = cdir.join(format!("ch{}.tsz", ch - 1));
        std::fs::write(&path, &encoded)
            .map_err(|e| format!("Cannot write {}: {e}", path.display()))?;
    }

    // Clear buffers for next chunk
    for col in columns.iter_mut() {
        col.clear();
    }

    Ok(())
}

/// Calculate total TSZ file size for a chunk (sum of all channel files).
fn chunk_tsz_size(csv_path: &Path, chunk_idx: u32, n_cols: usize) -> u64 {
    let cdir = chunk_dir(csv_path, chunk_idx);
    let mut total = 0u64;
    for ch in 0..n_cols {
        if ch == 0 { continue; }
        let path = cdir.join(format!("ch{}.tsz", ch - 1));
        if let Ok(meta) = std::fs::metadata(&path) {
            total += meta.len();
        }
    }
    total
}

/// Fast f64 parser that handles common edge cases.
#[inline]
fn fast_parse_f64(s: &str) -> f64 {
    s.parse::<f64>().unwrap_or(f64::NAN)
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
