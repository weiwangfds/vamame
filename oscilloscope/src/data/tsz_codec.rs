//! Lightweight Gorilla-inspired codec for f64 waveform data.
//!
//! Instead of using the `tsz` crate (which expects integer timestamps),
//! this module implements a custom encoding that works directly with f64
//! bit patterns:
//!
//! - **Timestamps:** delta-of-delta encoding on i64 bitcast of f64.
//!   For regularly-sampled data, delta-of-delta is nearly all zeros.
//! - **Values:** XOR encoding on u64 bitcast of f64.
//!   Adjacent waveform values are similar, so XOR has many leading zeros.
//! - Both streams are compressed with zstd.
//!
//! Format per channel:
//! ```text
//! [8 bytes] first_timestamp (f64, raw)
//! [8 bytes] first_value (f64, raw)
//! [4 bytes] n_rows (u32)
//! [4 bytes] zstd_timestamps_len (u32)
//! [N bytes] zstd-compressed delta-of-delta timestamps
//! [4 bytes] zstd_values_len (u32)
//! [N bytes] zstd-compressed XOR values
//! ```

/// Encode a single channel's (timestamps, values) into compressed bytes.
pub fn encode_channel(timestamps: &[f64], values: &[f64]) -> Vec<u8> {
    assert_eq!(timestamps.len(), values.len());
    let n = timestamps.len();
    if n == 0 {
        return Vec::new();
    }

    // ── Encode timestamps: delta-of-delta on i64 bitcast ──
    let ts_bits: Vec<i64> = timestamps.iter().map(|t| t.to_bits() as i64).collect();
    let mut ts_deltas = Vec::with_capacity(n - 1);
    if n > 1 {
        ts_deltas.push(ts_bits[1].wrapping_sub(ts_bits[0]));
        for i in 2..n {
            let delta = ts_bits[i].wrapping_sub(ts_bits[i - 1]);
            let prev_delta = ts_bits[i - 1].wrapping_sub(ts_bits[i - 2]);
            ts_deltas.push(delta.wrapping_sub(prev_delta)); // delta-of-delta
        }
    }
    let ts_bytes: &[u8] = bytemuck::cast_slice(&ts_deltas);
    let ts_compressed = zstd::encode_all(ts_bytes, 3).unwrap_or_default();

    // ── Encode values: XOR of consecutive u64 bit patterns ──
    let val_bits: Vec<u64> = values.iter().map(|v| v.to_bits()).collect();
    let mut val_xor = Vec::with_capacity(n - 1);
    for i in 1..n {
        val_xor.push(val_bits[i] ^ val_bits[i - 1]);
    }
    let val_bytes: &[u8] = bytemuck::cast_slice(&val_xor);
    let val_compressed = zstd::encode_all(val_bytes, 3).unwrap_or_default();

    // ── Pack into binary format ──
    let mut out = Vec::with_capacity(28 + ts_compressed.len() + val_compressed.len());
    out.extend_from_slice(&timestamps[0].to_le_bytes());
    out.extend_from_slice(&values[0].to_le_bytes());
    out.extend_from_slice(&(n as u32).to_le_bytes());
    out.extend_from_slice(&(ts_compressed.len() as u32).to_le_bytes());
    out.extend_from_slice(&ts_compressed);
    out.extend_from_slice(&(val_compressed.len() as u32).to_le_bytes());
    out.extend_from_slice(&val_compressed);

    out
}

/// Decode compressed bytes back to (timestamps, values).
pub fn decode_channel(data: &[u8]) -> (Vec<f64>, Vec<f64>) {
    if data.len() < 28 {
        return (Vec::new(), Vec::new());
    }

    let mut pos = 0;

    let first_ts = f64::from_le_bytes(data[pos..pos + 8].try_into().unwrap());
    pos += 8;
    let first_val = f64::from_le_bytes(data[pos..pos + 8].try_into().unwrap());
    pos += 8;
    let n = u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap()) as usize;
    pos += 4;

    if n == 0 {
        return (Vec::new(), Vec::new());
    }

    // Decode timestamps
    let ts_len = u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap()) as usize;
    pos += 4;
    let ts_compressed = &data[pos..pos + ts_len];
    pos += ts_len;

    let mut timestamps = Vec::with_capacity(n);
    timestamps.push(first_ts);

    if n > 1 {
        let ts_deltas: Vec<i64> = zstd::decode_all(ts_compressed)
            .ok()
            .map(|decompressed| {
                bytemuck::cast_slice::<u8, i64>(&decompressed).to_vec()
            })
            .unwrap_or_default();

        if !ts_deltas.is_empty() {
            // First delta is direct
            let mut prev_bits = first_ts.to_bits() as i64;
            let mut prev_delta = ts_deltas[0];
            prev_bits = prev_bits.wrapping_add(prev_delta);
            timestamps.push(f64::from_bits(prev_bits as u64));

            // Remaining are delta-of-delta
            for &dod in &ts_deltas[1..] {
                prev_delta = prev_delta.wrapping_add(dod);
                prev_bits = prev_bits.wrapping_add(prev_delta);
                timestamps.push(f64::from_bits(prev_bits as u64));
            }
        }
    }

    // Decode values
    let val_len = u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap()) as usize;
    pos += 4;
    let val_compressed = &data[pos..pos + val_len];

    let mut values = Vec::with_capacity(n);
    values.push(first_val);

    if n > 1 {
        let val_xor: Vec<u64> = zstd::decode_all(val_compressed)
            .ok()
            .map(|decompressed| {
                bytemuck::cast_slice::<u8, u64>(&decompressed).to_vec()
            })
            .unwrap_or_default();

        let mut prev_bits = first_val.to_bits();
        for &xor in &val_xor {
            prev_bits ^= xor;
            values.push(f64::from_bits(prev_bits));
        }
    }

    (timestamps, values)
}

/// Decode into Vec<[f64; 2]> (time, value) pairs.
pub fn decode_channel_points(data: &[u8]) -> Vec<[f64; 2]> {
    let (timestamps, values) = decode_channel(data);
    timestamps.into_iter().zip(values).map(|(t, v)| [t, v]).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_empty() {
        let (ts, vs) = decode_channel(&[]);
        assert!(ts.is_empty());
        assert!(vs.is_empty());
    }

    #[test]
    fn roundtrip_single_point() {
        let timestamps = vec![1.0f64];
        let values = vec![42.0f64];
        let encoded = encode_channel(&timestamps, &values);
        assert!(!encoded.is_empty());
        let (ts, vs) = decode_channel(&encoded);
        assert_eq!(ts.len(), 1);
        assert!((ts[0] - 1.0).abs() < f64::EPSILON);
        assert!((vs[0] - 42.0).abs() < f64::EPSILON);
    }

    #[test]
    fn roundtrip_waveform() {
        let n = 100_000;
        let timestamps: Vec<f64> = (0..n).map(|i| 1e-6 + i as f64 * 1e-9).collect();
        let values: Vec<f64> = (0..n).map(|i| (i as f64 * 0.001).sin()).collect();

        let encoded = encode_channel(&timestamps, &values);
        let (ts, vs) = decode_channel(&encoded);

        assert_eq!(ts.len(), n);
        assert_eq!(vs.len(), n);

        // Verify first and last points
        assert!((ts[0] - timestamps[0]).abs() < f64::EPSILON);
        assert!((vs[0] - values[0]).abs() < f64::EPSILON);
        assert!((ts[n - 1] - timestamps[n - 1]).abs() < f64::EPSILON);
        assert!((vs[n - 1] - values[n - 1]).abs() < f64::EPSILON);

        // Check compression ratio
        let raw_size = n * 16;
        let ratio = raw_size as f64 / encoded.len() as f64;
        eprintln!(
            "Compression: {} raw bytes → {} encoded bytes, ratio: {:.1}:1",
            raw_size,
            encoded.len(),
            ratio
        );
        assert!(ratio > 2.0, "Expected at least 2:1 compression, got {:.1}:1", ratio);
    }

    #[test]
    fn decode_points_matches() {
        let timestamps = vec![1.0, 2.0, 3.0];
        let values = vec![10.0, 20.0, 30.0];
        let encoded = encode_channel(&timestamps, &values);
        let points = decode_channel_points(&encoded);
        assert_eq!(points.len(), 3);
        assert!((points[0][0] - 1.0).abs() < f64::EPSILON);
        assert!((points[0][1] - 10.0).abs() < f64::EPSILON);
        assert!((points[2][0] - 3.0).abs() < f64::EPSILON);
        assert!((points[2][1] - 30.0).abs() < f64::EPSILON);
    }

    #[test]
    fn roundtrip_negative_timestamps() {
        let timestamps = vec![-1.32e-6, -1.31e-6, -1.30e-6];
        let values = vec![-0.036, -0.035, -0.034];
        let encoded = encode_channel(&timestamps, &values);
        let (ts, vs) = decode_channel(&encoded);
        assert_eq!(ts.len(), 3);
        for i in 0..3 {
            assert!((ts[i] - timestamps[i]).abs() < 1e-20);
            assert!((vs[i] - values[i]).abs() < 1e-20);
        }
    }

    #[test]
    fn roundtrip_randomish_data() {
        let n = 10_000;
        let timestamps: Vec<f64> = (0..n).map(|i| i as f64 * 7.3e-9).collect();
        let values: Vec<f64> = (0..n)
            .map(|i| (i as f64 * 0.01).sin() * 0.5 + (i as f64 * 0.037).cos() * 0.3)
            .collect();

        let encoded = encode_channel(&timestamps, &values);
        let (ts, vs) = decode_channel(&encoded);

        assert_eq!(ts.len(), n);
        for i in 0..n {
            assert!((ts[i] - timestamps[i]).abs() < 1e-20, "ts[{}]: got {}, expected {}", i, ts[i], timestamps[i]);
            assert!((vs[i] - values[i]).abs() < 1e-20, "vs[{}]: got {}, expected {}", i, vs[i], values[i]);
        }
    }
}
