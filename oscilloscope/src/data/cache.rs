//! CSV → Parquet cache layer.
//!
//! On first open, the CSV is converted to a compressed Parquet file
//! stored next to the original (`<name>.oscv/<stem>/data.parquet`).
//!
//! Conversion strategy:
//! 1. Read CSV with all columns as **String** (avoids parse errors from whitespace)
//! 2. In the lazy frame: `strip_chars` → `cast(Float64)` for every column
//! 3. Stream to Parquet via `sink_parquet` (bounded memory)
//!
//! Parquet's row-group statistics enable predicate pushdown so that
//! `scan_parquet` with a time filter reads only the relevant row groups.

use polars::prelude::*;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Name of the cache directory placed next to the source CSV.
const CACHE_DIR: &str = ".oscv";

/// Rows per Parquet row-group.
const ROWS_PER_GROUP: usize = 100_000;

// ─── Metadata ────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize)]
pub struct CacheMeta {
    pub md5: String,
    pub n_rows: usize,
    pub n_cols: usize,
    pub columns: Vec<String>,
    pub x_min: f64,
    pub x_max: f64,
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

pub fn parquet_path(csv_path: &Path) -> PathBuf {
    cache_dir(csv_path).join("data.parquet")
}

pub fn meta_path(csv_path: &Path) -> PathBuf {
    cache_dir(csv_path).join("metadata.json")
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
    if !parquet_path(csv_path).exists() {
        return None;
    }
    Some(meta)
}

// ─── Conversion ──────────────────────────────────────────────────────

/// Convert a CSV file to cached Parquet.
///
/// Reads all columns as String (safe for whitespace-padded / malformed values),
/// then in the lazy frame strips leading/trailing whitespace and casts to Float64.
/// Uses `sink_parquet` for streaming write with bounded memory.
pub fn convert_csv_to_parquet(
    csv_path: &Path,
    progress: &dyn Fn(usize, u64, u64),
) -> Result<CacheMeta, String> {
    let total_size = std::fs::metadata(csv_path)
        .map_err(|e| format!("Cannot stat {}: {e}", csv_path.display()))?
        .len();

    let dir = cache_dir(csv_path);
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("Cannot create {}: {e}", dir.display()))?;

    progress(0, 0, total_size);

    // ── Step 1: detect column count from first line ──
    let n_cols = count_csv_columns(csv_path)?;

    // ── Step 2: read CSV with all columns as String ──
    // This avoids any parse errors from whitespace-padded values.
    let fields: Vec<Field> = (1..=n_cols)
        .map(|i| Field::new(PlSmallStr::from(format!("column_{i}")), DataType::String))
        .collect();
    let schema_override = Arc::new(Schema::from_iter(fields));

    let mut lf = LazyCsvReader::new(csv_path)
        .with_has_header(false)
        .with_dtype_overwrite(Some(schema_override))
        .with_ignore_errors(true)
        .with_truncate_ragged_lines(true)
        .finish()
        .map_err(|e| format!("CSV open error: {e}"))?;

    let schema = lf
        .collect_schema()
        .map_err(|e| format!("Schema error: {e}"))?;
    let columns: Vec<String> = schema.iter_names().map(|s| s.to_string()).collect();

    // ── Step 3: strip whitespace + cast to Float64 ──
    // strip_chars(lit(NULL)) strips all whitespace characters.
    let strip_cast: Vec<Expr> = columns
        .iter()
        .map(|name| {
            col(name)
                .str()
                .strip_chars(Null {}.lit())
                .cast(DataType::Float64)
                .alias(name)
        })
        .collect();
    let lf = lf.select(strip_cast);

    // ── Step 4: stream to Parquet ──
    let pp = parquet_path(csv_path);

    let sink_options = ParquetWriteOptions {
        compression: ParquetCompression::Zstd(None),
        statistics: StatisticsOptions::default(),
        row_group_size: Some(ROWS_PER_GROUP),
        data_page_size: None,
        maintain_order: true,
    };

    lf.sink_parquet(&pp, sink_options)
        .map_err(|e| format!("Parquet write error: {e}"))?;

    progress(0, total_size, total_size);

    // ── Step 5: compute metadata from Parquet ──
    let time_col = &columns[0];
    let meta_df = LazyFrame::scan_parquet(&pp, ScanArgsParquet::default())
        .map_err(|e| format!("Parquet scan error: {e}"))?
        .select([
            len().alias("n_rows"),
            col(time_col).min().alias("x_min"),
            col(time_col).max().alias("x_max"),
        ])
        .collect()
        .map_err(|e| format!("Metadata scan error: {e}"))?;

    let n_rows = meta_df
        .column("n_rows")
        .ok()
        .and_then(|c| c.idx().ok())
        .and_then(|ca| ca.get(0))
        .unwrap_or(0) as usize;

    let x_min = meta_df
        .column("x_min")
        .ok()
        .and_then(|c| c.f64().ok())
        .and_then(|ca| ca.get(0))
        .unwrap_or(0.0);

    let x_max = meta_df
        .column("x_max")
        .ok()
        .and_then(|c| c.f64().ok())
        .and_then(|ca| ca.get(0))
        .unwrap_or(0.0);

    // ── Step 6: save metadata ──
    let md5 = fingerprint(csv_path)?;
    let meta = CacheMeta {
        md5,
        n_rows,
        n_cols,
        columns,
        x_min,
        x_max,
    };
    let json =
        serde_json::to_string_pretty(&meta).map_err(|e| format!("JSON error: {e}"))?;
    std::fs::write(meta_path(csv_path), json)
        .map_err(|e| format!("Cannot write metadata: {e}"))?;

    eprintln!(
        "Converted: {} rows, {} cols, range [{:.6e}, {:.6e}]",
        n_rows, n_cols, x_min, x_max
    );

    Ok(meta)
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
