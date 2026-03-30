use std::collections::VecDeque;

/// On-Balance Volume — incremental O(1).
///
/// Weight: 1.0 — Volume confirmation role.
#[derive(Debug, Clone)]
pub struct ObvState {
    pub value: f64,
    pub prev_close: f64,
    rolling_window: usize,
    /// Stores the **signed** delta each bar (positive = price up, negative = price down).
    /// Used to determine OBV direction in confirms_move.
    deltas: VecDeque<f64>,
    /// Rolling average of |delta| — used as a threshold to filter noise.
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

        // Store the signed delta; rolling_avg tracks magnitude only.
        self.deltas.push_back(delta);
        if self.deltas.len() > self.rolling_window {
            self.deltas.pop_front();
        }
        self.rolling_avg = if self.deltas.is_empty() {
            0.0
        } else {
            self.deltas.iter().map(|d| d.abs()).sum::<f64>() / self.deltas.len() as f64
        };

        self.value
    }

    /// True if the last OBV delta confirms the expected price direction.
    ///
    /// `price_dir`: +1 for bullish, -1 for bearish.
    ///
    /// Uses the actual signed delta — NOT the absolute value — so direction is
    /// preserved. Also requires the move to be above 0.3% of the rolling average
    /// to filter out noise.
    pub fn confirms_move(&self, price_dir: i8) -> bool {
        let last_delta = match self.deltas.back() {
            Some(&d) => d,
            None => return false,
        };

        // Direction must match
        let dir_matches = match price_dir {
            1 => last_delta > 0.0,
            -1 => last_delta < 0.0,
            _ => return false,
        };

        // Magnitude must be meaningful (above 0.3% of rolling average)
        let above_threshold = last_delta.abs() > self.rolling_avg * 0.003;

        dir_matches && above_threshold
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

    #[test]
    fn test_confirms_move_bullish() {
        let mut obv = ObvState::new(5);
        for _ in 0..5 {
            obv.update(100.0, 1000.0); // seed rolling avg
            obv.update(101.0, 1000.0);
        }
        // Last delta was positive (price went up)
        assert!(obv.confirms_move(1), "Should confirm bullish move");
        assert!(!obv.confirms_move(-1), "Should NOT confirm bearish when last delta > 0");
    }

    #[test]
    fn test_confirms_move_bearish() {
        let mut obv = ObvState::new(5);
        for _ in 0..5 {
            obv.update(100.0, 1000.0);
            obv.update(99.0, 1000.0); // seed with down moves
        }
        assert!(obv.confirms_move(-1), "Should confirm bearish move");
        assert!(!obv.confirms_move(1), "Should NOT confirm bullish when last delta < 0");
    }
}
