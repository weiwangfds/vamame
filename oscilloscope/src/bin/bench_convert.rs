use std::time::Instant;

use oscilloscope::data::cache;

fn main() {
    let path = std::env::args().nth(1).expect("Usage: bench_convert <csv_path>");
    let csv_path = std::path::Path::new(&path);

    let total_size = std::fs::metadata(csv_path).unwrap().len();
    println!("File: {path}");
    println!("Size: {:.2} GB", total_size as f64 / 1e9);

    // Remove existing cache if any
    let cache_dir = cache::cache_dir(csv_path);
    if cache_dir.exists() {
        println!("Removing existing cache: {}", cache_dir.display());
        std::fs::remove_dir_all(&cache_dir).ok();
    }

    let start = Instant::now();
    let result = cache::convert_csv_to_tsz(csv_path, &|rows, _bytes, _total| {
        if rows > 0 && rows % 1_000_000 == 0 {
            let elapsed = start.elapsed().as_secs_f64();
            let rows_per_sec = rows as f64 / elapsed;
            println!("  {rows} rows ({:.0} rows/sec)", rows_per_sec);
        }
    });

    let elapsed = start.elapsed();
    match result {
        Ok(meta) => {
            println!("Conversion complete in {:.1}s", elapsed.as_secs_f64());
            println!("Rows: {}, Cols: {}", meta.n_rows, meta.n_cols);
            println!("Speed: {:.2} GB/s", total_size as f64 / 1e9 / elapsed.as_secs_f64());
            // Calculate TSZ cache size
            let cache_size: u64 = walk_dir_size(&cache::chunks_dir(csv_path));
            println!("TSZ cache size: {:.2} GB", cache_size as f64 / 1e9);
        }
        Err(e) => {
            println!("Error after {:.1}s: {e}", elapsed.as_secs_f64());
        }
    }
}

fn walk_dir_size(path: &std::path::Path) -> u64 {
    let mut total = 0u64;
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                total += walk_dir_size(&p);
            } else if let Ok(meta) = std::fs::metadata(&p) {
                total += meta.len();
            }
        }
    }
    total
}
