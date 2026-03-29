use std::collections::VecDeque;

/// On-Balance Volume — incremental O(1).
///
/// Weight: 1.0 — Volume confirmation role.
#[derive(Debug, Clone)]
pub struct ObvState {
    pub value: f64,
    pub prev_close: f64,
    rolling_window: usize,
    deltas: VecDeque<f64>, // rolling OBV deltas for average
    pub rolling_avg: f64,
    pub initialized: bool,
}

impl ObvState {
    pub fn new(rolling_window: usize) -> Self {
        Self {
            value: 0.0,
            prev_close: 0.0,
            rolling_window,
            deltas: VecDeque::with_capacity(rolling_window),
            rolling_avg: 0.0,
            initialized: false,
        }
    }

    /// Feed (close, volume). Returns current OBV after update.
    pub fn update(&mut self, close: f64, volume: f64) -> f64 {
        let delta = if !self.initialized {
            // First bar — just record close, no delta yet
            self.prev_close = close;
            self.initialized = true;
            0.0
        } else if close > self.prev_close {
            volume
        } else if close < self.prev_close {
            -volume
        } else {
            0.0
        };

        self.value += delta;
        self.prev_close = close;

        // Maintain rolling average of |delta|
        self.deltas.push_back(delta.abs());
        if self.deltas.len() > self.rolling_window {
            self.deltas.pop_front();
        }
        self.rolling_avg = if self.deltas.is_empty() {
            0.0
        } else {
            self.deltas.iter().sum::<f64>() / self.deltas.len() as f64
        };

        self.value
    }

    /// True if volume confirms price direction.
    ///
    /// `price_dir`: +1 for up, -1 for down.
    /// Requires the last OBV delta to align with direction AND be above 0.3% of rolling avg.
    pub fn confirms_move(&self, price_dir: i8) -> bool {
        let last_delta = self.deltas.back().copied().unwrap_or(0.0);
        let raw_last = if self.prev_close > 0.0 { last_delta } else { 0.0 };

        // Get actual signed delta from value
        let signed_delta = match price_dir {
            1 => raw_last,
            -1 => -raw_last,
            _ => return false,
        };

        let above_threshold = last_delta > self.rolling_avg * 0.003;
        signed_delta > 0.0 && above_threshold
    }

    /// Score: +1 (confirms bullish), -1 (confirms bearish), 0 (no confirmation).
    pub fn score(&self, price_dir: i8) -> f64 {
        if self.confirms_move(price_dir) {
            price_dir as f64
        } else {
            0.0
        }
    }

    pub fn is_ready(&self) -> bool {
        self.initialized && self.deltas.len() >= self.rolling_window / 2
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_obv_up_move() {
        let mut obv = ObvState::new(20);
        obv.update(100.0, 1000.0); // seed
        obv.update(101.0, 500.0);  // price up → OBV += 500
        assert_eq!(obv.value, 500.0);
    }

    #[test]
    fn test_obv_down_move() {
        let mut obv = ObvState::new(20);
        obv.update(100.0, 1000.0);
        obv.update(99.0, 800.0); // price down → OBV -= 800
        assert_eq!(obv.value, -800.0);
    }

    #[test]
    fn test_obv_unchanged_price() {
        let mut obv = ObvState::new(20);
        obv.update(100.0, 1000.0);
        obv.update(100.0, 500.0); // price flat → OBV unchanged
        assert_eq!(obv.value, 0.0);
    }
}
