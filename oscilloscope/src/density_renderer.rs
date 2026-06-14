use egui::{Color32, ColorImage, TextureHandle, TextureId};

/// CPU-based density renderer using the difference-array technique
/// (adapted from turboplot's CpuRenderer). For each pixel column it finds
/// which line-segments overlap, marks their vertical extent in a difference
/// array, then prefix-sums to obtain per-pixel overlap counts.
///
/// Complexity: O(W · log N + N) for N points across W pixel columns.
///
/// The texture is rendered with a 50% x-axis margin so small pans reuse
/// the cached texture without recomputation.
pub struct DensityCache {
    texture: Option<TextureHandle>,
    cached: CachedState,
}

struct CachedState {
    n_points: usize,
    x_min: f64,
    x_max: f64,
    y_min_bits: u64,
    y_max_bits: u64,
    width: usize,
    height: usize,
}

pub struct DensityResult {
    pub texture_id: TextureId,
    pub cached_x_min: f64,
    pub cached_x_max: f64,
}

impl DensityCache {
    pub fn new() -> Self {
        Self {
            texture: None,
            cached: CachedState {
                n_points: 0,
                x_min: f64::INFINITY,
                x_max: f64::NEG_INFINITY,
                y_min_bits: 0,
                y_max_bits: 0,
                width: 0,
                height: 0,
            },
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn ensure_texture(
        &mut self,
        ctx: &egui::Context,
        points: &[[f64; 2]],
        x_min: f64,
        x_max: f64,
        y_min: f64,
        y_max: f64,
        width: usize,
        height: usize,
        color: Color32,
    ) -> Option<DensityResult> {
        if points.len() < 2 || width == 0 || height == 0 || x_min >= x_max || y_min >= y_max {
            return None;
        }

        let n_points = points.len();
        let y_min_bits = y_min.to_bits();
        let y_max_bits = y_max.to_bits();

        let cache_valid = self.texture.is_some()
            && self.cached.n_points == n_points
            && self.cached.x_min <= x_min
            && self.cached.x_max >= x_max
            && self.cached.y_min_bits == y_min_bits
            && self.cached.y_max_bits == y_max_bits
            && self.cached.width == width
            && self.cached.height == height;

        if cache_valid {
            return self.texture.as_ref().map(|t| DensityResult {
                texture_id: t.id(),
                cached_x_min: self.cached.x_min,
                cached_x_max: self.cached.x_max,
            });
        }

        let x_span = x_max - x_min;
        let margin = x_span * 0.5;
        let exp_x_min = x_min - margin;
        let exp_x_max = x_max + margin;

        let density = compute_density(points, exp_x_min, exp_x_max, y_min, y_max, width, height);
        let img = density_to_image(&density, width, height, color);

        let tex = self.texture.get_or_insert_with(|| {
            ctx.load_texture("density-waveform", img.clone(), egui::TextureOptions::LINEAR)
        });
        tex.set(img, egui::TextureOptions::LINEAR);

        self.cached = CachedState {
            n_points,
            x_min: exp_x_min,
            x_max: exp_x_max,
            y_min_bits,
            y_max_bits,
            width,
            height,
        };

        Some(DensityResult {
            texture_id: tex.id(),
            cached_x_min: exp_x_min,
            cached_x_max: exp_x_max,
        })
    }
}

fn compute_density(
    points: &[[f64; 2]],
    x_min: f64,
    x_max: f64,
    y_min: f64,
    y_max: f64,
    width: usize,
    height: usize,
) -> Vec<u32> {
    let mut result = vec![0u32; width * height];
    let mut diff = vec![0i32; height + 1];

    let x_span = x_max - x_min;
    let y_span = y_max - y_min;
    if x_span <= 0.0 || y_span <= 0.0 {
        return result;
    }

    // Precompute pixel-mapped y for every point to avoid repeated division.
    let y_px: Vec<i32> = points
        .iter()
        .map(|p| ((p[1] - y_min) / y_span * height as f64) as i32)
        .collect();

    for px in 0..width {
        for d in diff.iter_mut() {
            *d = 0;
        }

        let col_x_min = x_min + (px as f64 / width as f64) * x_span;
        let col_x_max = x_min + ((px + 1) as f64 / width as f64) * x_span;

        let i_start = points.partition_point(|p| p[0] < col_x_min);
        let i_end = (points.partition_point(|p| p[0] <= col_x_max))
            .min(points.len().saturating_sub(1));

        if i_start >= i_end {
            continue;
        }

        for i in i_start..i_end {
            let (ya, yb) = (y_px[i], y_px[i + 1]);
            let (lo, hi) = if ya <= yb { (ya, yb) } else { (yb, ya) };
            let lo = lo.clamp(0, height as i32 - 1) as usize;
            let hi = hi.clamp(0, height as i32 - 1) as usize;
            diff[lo] += 1;
            diff[hi + 1] -= 1;
        }

        let mut density = 0i32;
        for py in 0..height {
            density += diff[py];
            result[py * width + px] = density.max(0) as u32;
        }
    }

    result
}

fn density_to_image(density: &[u32], width: usize, height: usize, color: Color32) -> ColorImage {
    let max_raw = density.iter().copied().max().unwrap_or(1).max(1);
    let log_max = (1.0 + max_raw as f64).ln();

    let pixels: Vec<Color32> = density
        .iter()
        .map(|&v| {
            if v == 0 {
                Color32::TRANSPARENT
            } else {
                let t = ((1.0 + v as f64).ln() / log_max).min(1.0) as f32;
                Color32::from_rgba_unmultiplied(
                    color.r(),
                    color.g(),
                    color.b(),
                    (t * 200.0).min(255.0) as u8,
                )
            }
        })
        .collect();

    ColorImage {
        size: [width, height],
        source_size: egui::Vec2::new(width as f32, height as f32),
        pixels,
    }
}

impl Default for DensityCache {
    fn default() -> Self {
        Self::new()
    }
}
