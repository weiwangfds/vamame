//! Cursor measurement state for manual ΔT / ΔV readings.

/// Which cursor mode is active.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CursorMode {
    Off,
    Vertical,  // Two vertical lines → ΔT, 1/ΔT
    Horizontal, // Two horizontal lines → ΔV
}

/// Which cursor the user is currently dragging.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CursorId {
    A,
    B,
}

/// Full cursor state.
#[derive(Clone, Debug)]
pub struct CursorState {
    pub mode: CursorMode,
    /// Position of cursor A (x for Vertical, y for Horizontal).
    pub pos_a: f64,
    /// Position of cursor B.
    pub pos_b: f64,
    /// Currently dragged cursor, if any.
    pub dragging: Option<CursorId>,
}

impl Default for CursorState {
    fn default() -> Self {
        Self {
            mode: CursorMode::Off,
            pos_a: 0.0,
            pos_b: 0.0,
            dragging: None,
        }
    }
}

impl CursorState {
    /// Absolute difference between the two cursors.
    pub fn delta(&self) -> f64 {
        (self.pos_b - self.pos_a).abs()
    }

    /// Switch cursor mode and place cursors symmetrically around the visible
    /// range centre.
    pub fn set_mode(&mut self, mode: CursorMode, range_min: f64, range_max: f64) {
        let mid = (range_min + range_max) / 2.0;
        let span = range_max - range_min;
        self.mode = mode;
        self.pos_a = mid - span * 0.15;
        self.pos_b = mid + span * 0.15;
        self.dragging = None;
    }
}
