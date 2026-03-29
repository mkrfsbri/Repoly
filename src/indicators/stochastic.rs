use std::collections::VecDeque;

/// Stochastic Oscillator — %K period 14, Smooth %K 3, %D 3. Incremental O(1).
///
/// Weight: 1.5 — Entry Trigger role.
#[derive(Debug, Clone)]
pub struct StochState {
    k_period: usize,
    smooth_k: usize,
    d_period: usize,
    highs: VecDeque<f64>,
    lows: VecDeque<f64>,
    raw_k_values: VecDeque<f64>, // for smooth %K
    k_values: VecDeque<f64>,     // smoothed %K, used for %D
    pub prev_k: f64,
    pub prev_d: f64,
    pub k: f64,
    pub d: f64,
    pub initialized: bool,
}

impl StochState {
    pub fn new(k_period: usize, smooth_k: usize, d_period: usize) -> Self {
        Self {
            k_period,
            smooth_k,
            d_period,
            highs: VecDeque::with_capacity(k_period),
            lows: VecDeque::with_capacity(k_period),
            raw_k_values: VecDeque::with_capacity(smooth_k),
            k_values: VecDeque::with_capacity(d_period),
            prev_k: 50.0,
            prev_d: 50.0,
            k: 50.0,
            d: 50.0,
            initialized: false,
        }
    }

    /// Feed HLC values. Returns `Some((k, d))` once warm-up is complete.
    pub fn update(&mut self, high: f64, low: f64, close: f64) -> Option<(f64, f64)> {
        // Maintain rolling high/low window
        self.highs.push_back(high);
        self.lows.push_back(low);
        if self.highs.len() > self.k_period {
            self.highs.pop_front();
            self.lows.pop_front();
        }

        if self.highs.len() < self.k_period {
            return None;
        }

        let highest_high = self.highs.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let lowest_low = self.lows.iter().cloned().fold(f64::INFINITY, f64::min);

        let raw_k = if (highest_high - lowest_low).abs() < 1e-12 {
            50.0
        } else {
            (close - lowest_low) / (highest_high - lowest_low) * 100.0
        };

        // Smooth %K (SMA of raw_k)
        self.raw_k_values.push_back(raw_k);
        if self.raw_k_values.len() > self.smooth_k {
            self.raw_k_values.pop_front();
        }
        if self.raw_k_values.len() < self.smooth_k {
            return None;
        }
        let smooth_k_val = self.raw_k_values.iter().sum::<f64>() / self.smooth_k as f64;

        // %D (SMA of smoothed %K)
        self.k_values.push_back(smooth_k_val);
        if self.k_values.len() > self.d_period {
            self.k_values.pop_front();
        }
        if self.k_values.len() < self.d_period {
            return None;
        }
        let d_val = self.k_values.iter().sum::<f64>() / self.d_period as f64;

        self.prev_k = self.k;
        self.prev_d = self.d;
        self.k = smooth_k_val;
        self.d = d_val;
        self.initialized = true;

        Some((smooth_k_val, d_val))
    }

    /// True if %K just crossed above %D from below in oversold zone (< 20).
    pub fn is_long_trigger(&self) -> bool {
        self.initialized && self.prev_k < self.prev_d && self.k > self.d && self.k < 20.0
    }

    /// True if %K just crossed below %D from above in overbought zone (> 80).
    pub fn is_short_trigger(&self) -> bool {
        self.initialized && self.prev_k > self.prev_d && self.k < self.d && self.k > 80.0
    }

    /// Score: +1.5 (long trigger), -1.5 (short trigger), 0 otherwise.
    pub fn score(&self) -> f64 {
        if self.is_long_trigger() {
            1.5
        } else if self.is_short_trigger() {
            -1.5
        } else {
            0.0
        }
    }

    pub fn is_oversold(&self) -> bool {
        self.initialized && self.k < 20.0
    }

    pub fn is_overbought(&self) -> bool {
        self.initialized && self.k > 80.0
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_bar(h: f64, l: f64, c: f64) -> (f64, f64, f64) {
        (h, l, c)
    }

    #[test]
    fn test_stoch_warm_up() {
        let mut s = StochState::new(5, 3, 3);
        // Need k_period + smooth_k + d_period - 2 bars minimum
        let bars: Vec<_> = (0..9).map(|i| make_bar(100.0 + i as f64, 95.0, 98.0)).collect();
        let mut result = None;
        for (h, l, c) in &bars {
            result = s.update(*h, *l, *c);
        }
        assert!(result.is_some(), "Should be initialized after 9 bars");
    }

    #[test]
    fn test_stoch_oversold_zone() {
        let mut s = StochState::new(5, 3, 3);
        // Downtrend: closes near lows (close = base - 4.5, barely above low = base - 5)
        for i in 0..15 {
            let base = 100.0 - i as f64;
            s.update(base, base - 5.0, base - 4.5);
        }
        assert!(s.initialized, "Should be initialized after 15 bars");
        // Close near the low → %K should be well below 50
        assert!(s.k < 30.0, "Expected low %K in downtrend, got {}", s.k);
    }
}
