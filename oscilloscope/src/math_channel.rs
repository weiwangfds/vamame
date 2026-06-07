//! Math channel definitions and computation.
//!
//! Math channels are virtual channels derived from one or two real channels
//! using arithmetic operations. Binary ops use Polars lazy; unary ops compute
//! from raw points in Rust for maximum API compatibility.

use polars::prelude::*;

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
        data: &WaveformData,
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

    /// Binary operations via Polars lazy.
    fn compute_binary(
        &self,
        data: &WaveformData,
        vis_x_min: f64,
        vis_x_max: f64,
        max_points: usize,
    ) -> Vec<[f64; 2]> {
        let time_col = data.time_col();
        let data_cols = data.data_cols();

        let col_a = match data_cols.get(self.source_a) {
            Some(c) => c.clone(),
            None => return Vec::new(),
        };
        let col_b = match data_cols.get(self.source_b.unwrap_or(0)) {
            Some(c) => c.clone(),
            None => return Vec::new(),
        };

        let result_expr = match self.operation {
            MathOp::Add => col(&col_a) + col(&col_b),
            MathOp::Subtract => col(&col_a) - col(&col_b),
            MathOp::Multiply => col(&col_a) * col(&col_b),
            _ => return Vec::new(),
        };

        let alias = "math_result";
        let result = data
            .df()
            .clone()
            .lazy()
            .filter(
                col(time_col)
                    .gt_eq(lit(vis_x_min))
                    .and(col(time_col).lt_eq(lit(vis_x_max))),
            )
            .select([col(time_col), result_expr.alias(alias)])
            .sort(
                [time_col],
                SortMultipleOptions::default().with_maintain_order(true),
            )
            .collect();

        let df = match result {
            Ok(df) => df,
            Err(e) => {
                eprintln!("Math binary error: {e}");
                return Vec::new();
            }
        };

        let df = subsample_if_needed(df, max_points);
        extract_points(&df, time_col, alias)
    }

    /// Unary operations from raw points (avoids Polars feature-flag issues).
    fn compute_unary(
        &self,
        data: &WaveformData,
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

/// Subsample a DataFrame if it has more rows than `max_points`.
fn subsample_if_needed(df: DataFrame, max_points: usize) -> DataFrame {
    if df.height() <= max_points {
        return df;
    }
    let step = (df.height() / max_points).max(1);
    let cols: Vec<Column> = df
        .get_columns()
        .iter()
        .map(|c| c.gather_every(step, 0))
        .collect();
    DataFrame::new(cols).unwrap_or(df)
}

/// Extract `[x, y]` points from a two-column DataFrame.
fn extract_points(df: &DataFrame, time_col: &str, data_col: &str) -> Vec<[f64; 2]> {
    let x_ca: &Float64Chunked = match df.column(time_col) {
        Ok(c) => match c.f64() {
            Ok(ca) => ca,
            Err(_) => return Vec::new(),
        },
        _ => return Vec::new(),
    };
    let y_ca: &Float64Chunked = match df.column(data_col) {
        Ok(c) => match c.f64() {
            Ok(ca) => ca,
            Err(_) => return Vec::new(),
        },
        _ => return Vec::new(),
    };

    let n = df.height();
    let mut points = Vec::with_capacity(n);
    for i in 0..n {
        if let (Some(x), Some(y)) = (x_ca.get(i), y_ca.get(i)) {
            points.push([x, y]);
        }
    }
    points
}
