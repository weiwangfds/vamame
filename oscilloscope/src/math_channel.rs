//! Math channel definitions and computation.
//!
//! Math channels are virtual channels derived from one or two real channels
//! using arithmetic operations. All operations work on raw points fetched
//! from the data layer.

use crate::data::WaveformData;

/// Supported math operations.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MathOp {
    Add,       // A + B
    Subtract,  // A - B
    Multiply,  // A × B
    Invert,    // -A
    Abs,       // |A|
    Derivative, // dA/dt
    Integral,  // ∫A dt
}

impl std::fmt::Display for MathOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Add => write!(f, "Add"),
            Self::Subtract => write!(f, "Subtract"),
            Self::Multiply => write!(f, "Multiply"),
            Self::Invert => write!(f, "Invert"),
            Self::Abs => write!(f, "Abs"),
            Self::Derivative => write!(f, "Derivative"),
            Self::Integral => write!(f, "Integral"),
        }
    }
}

impl MathOp {
    pub fn needs_source_b(&self) -> bool {
        matches!(self, Self::Add | Self::Subtract | Self::Multiply)
    }

    pub fn all() -> &'static [MathOp] {
        &[
            MathOp::Add,
            MathOp::Subtract,
            MathOp::Multiply,
            MathOp::Invert,
            MathOp::Abs,
            MathOp::Derivative,
            MathOp::Integral,
        ]
    }
}

/// Definition of a math channel.
#[derive(Clone, Debug)]
pub struct MathChannelDef {
    pub operation: MathOp,
    pub source_a: usize,
    pub source_b: Option<usize>,
}

impl MathChannelDef {
    /// Build a display name like "CH1+CH2" or "d(CH1)/dt".
    pub fn display_name(&self, channel_names: &[String]) -> String {
        let a = channel_names
            .get(self.source_a)
            .map(|s| s.as_str())
            .unwrap_or("?");
        match self.operation {
            MathOp::Add => {
                let b = channel_names
                    .get(self.source_b.unwrap_or(0))
                    .map(|s| s.as_str())
                    .unwrap_or("?");
                format!("{}+{}", a, b)
            }
            MathOp::Subtract => {
                let b = channel_names
                    .get(self.source_b.unwrap_or(0))
                    .map(|s| s.as_str())
                    .unwrap_or("?");
                format!("{}-{}", a, b)
            }
            MathOp::Multiply => {
                let b = channel_names
                    .get(self.source_b.unwrap_or(0))
                    .map(|s| s.as_str())
                    .unwrap_or("?");
                format!("{}*{}", a, b)
            }
            MathOp::Invert => format!("-{}", a),
            MathOp::Abs => format!("|{}|", a),
            MathOp::Derivative => format!("d({})/dt", a),
            MathOp::Integral => format!("int({})", a),
        }
    }

    /// Compute the math channel data for the visible x-range.
    pub fn compute(
        &self,
        data: &mut WaveformData,
        vis_x_min: f64,
        vis_x_max: f64,
        max_points: usize,
    ) -> Vec<[f64; 2]> {
        if self.operation.needs_source_b() {
            self.compute_binary(data, vis_x_min, vis_x_max, max_points)
        } else {
            self.compute_unary(data, vis_x_min, vis_x_max, max_points)
        }
    }

    /// Binary operations: fetch raw points from both channels, align by index.
    fn compute_binary(
        &self,
        data: &mut WaveformData,
        vis_x_min: f64,
        vis_x_max: f64,
        max_points: usize,
    ) -> Vec<[f64; 2]> {
        let pts_a = data.get_raw_points(self.source_a, vis_x_min, vis_x_max, max_points);
        let pts_b = data.get_raw_points(self.source_b.unwrap_or(0), vis_x_min, vis_x_max, max_points);

        if pts_a.is_empty() || pts_b.is_empty() {
            return Vec::new();
        }

        // Use the shorter length to avoid out-of-bounds
        let n = pts_a.len().min(pts_b.len());
        let mut out = Vec::with_capacity(n);

        for i in 0..n {
            let t = pts_a[i][0]; // use time from channel A
            let va = pts_a[i][1];
            let vb = pts_b[i][1];
            let result = match self.operation {
                MathOp::Add => va + vb,
                MathOp::Subtract => va - vb,
                MathOp::Multiply => va * vb,
                _ => unreachable!(),
            };
            out.push([t, result]);
        }

        out
    }

    /// Unary operations from raw points.
    fn compute_unary(
        &self,
        data: &mut WaveformData,
        vis_x_min: f64,
        vis_x_max: f64,
        max_points: usize,
    ) -> Vec<[f64; 2]> {
        let pts = data.get_raw_points(self.source_a, vis_x_min, vis_x_max, max_points);
        if pts.is_empty() {
            return Vec::new();
        }

        match self.operation {
            MathOp::Invert => pts.into_iter().map(|p| [p[0], -p[1]]).collect(),
            MathOp::Abs => pts.into_iter().map(|p| [p[0], p[1].abs()]).collect(),
            MathOp::Derivative => {
                let mut out = Vec::with_capacity(pts.len().saturating_sub(1));
                for i in 1..pts.len() {
                    let dt = pts[i][0] - pts[i - 1][0];
                    if dt.abs() > f64::EPSILON {
                        let dy = pts[i][1] - pts[i - 1][1];
                        let t_mid = (pts[i][0] + pts[i - 1][0]) / 2.0;
                        out.push([t_mid, dy / dt]);
                    }
                }
                out
            }
            MathOp::Integral => {
                let mut out = Vec::with_capacity(pts.len());
                let mut cumsum = 0.0;
                for i in 0..pts.len() {
                    if i > 0 {
                        let dt = pts[i][0] - pts[i - 1][0];
                        let avg_y = (pts[i][1] + pts[i - 1][1]) / 2.0;
                        cumsum += avg_y * dt;
                    }
                    out.push([pts[i][0], cumsum]);
                }
                out
            }
            _ => pts,
        }
    }
}
