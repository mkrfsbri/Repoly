/// RSI — Wilder's Smoothed RS, incremental O(1).
///
/// Weight: 1.0
/// Role  : Context filter
#[derive(Debug, Clone)]
pub struct RsiState {
    pub period: usize,
    pub gains_avg: f64,
    pub losses_avg: f64,
    pub prev_close: f64,
    pub initialized: bool,
    samples: usize,
    seed_gains: f64,
    seed_losses: f64,
}

impl RsiState {
    pub fn new(period: usize) -> Self {
        assert!(period > 0);
        Self {
            period,
            gains_avg: 0.0,
            losses_avg: 0.0,
            prev_close: 0.0,
            initialized: false,
            samples: 0,
            seed_gains: 0.0,
            seed_losses: 0.0,
        }
    }

    /// Feed a close price. Returns `Some(rsi)` once warm-up completes.
    pub fn update(&mut self, close: f64) -> Option<f64> {
        if self.samples == 0 {
            self.prev_close = close;
            self.samples += 1;
            return None;
        }

        let change = close - self.prev_close;
        let gain = change.max(0.0);
        let loss = (-change).max(0.0);
        self.prev_close = close;
        self.samples += 1;

        if !self.initialized {
            self.seed_gains += gain;
            self.seed_losses += loss;

            if self.samples > self.period {
                // Enough samples: compute initial averages
                self.gains_avg = self.seed_gains / self.period as f64;
                self.losses_avg = self.seed_losses / self.period as f64;
                self.initialized = true;
                return Some(self.compute());
            }
            return None;
        }

        // Wilder's smoothing: avg_t = (avg_{t-1} * (n-1) + x) / n
        let n = self.period as f64;
        self.gains_avg = (self.gains_avg * (n - 1.0) + gain) / n;
        self.losses_avg = (self.losses_avg * (n - 1.0) + loss) / n;
        Some(self.compute())
    }

    fn compute(&self) -> f64 {
        if self.losses_avg < 1e-12 {
            return 100.0;
        }
        let rs = self.gains_avg / self.losses_avg;
        100.0 - (100.0 / (1.0 + rs))
    }

    pub fn get(&self) -> Option<f64> {
        if self.initialized {
            Some(self.compute())
        } else {
            None
        }
    }

    /// Score: +1 (oversold, bullish context), -1 (overbought, bearish), 0 (neutral).
    pub fn score(&self) -> f64 {
        match self.get() {
            Some(v) if v < 40.0 => 1.0,
            Some(v) if v > 70.0 => -1.0,
            Some(_) => 0.0,
            None => 0.0,
        }
    }

    pub fn is_oversold(&self) -> bool {
        self.get().map(|v| v < 40.0).unwrap_or(false)
    }

    pub fn is_overbought(&self) -> bool {
        self.get().map(|v| v > 60.0).unwrap_or(false)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rsi_warm_up() {
        let mut rsi = RsiState::new(14);
        // First 14 changes need at least 15 closes (first close sets prev)
        for i in 0..14 {
            assert!(rsi.update(100.0 + i as f64).is_none());
        }
        // 15th close triggers initialization
        let v = rsi.update(114.0);
        assert!(v.is_some());
        let v = v.unwrap();
        assert!(v > 0.0 && v <= 100.0);
    }

    #[test]
    fn test_rsi_all_gains() {
        let mut rsi = RsiState::new(3);
        // Strictly increasing: losses_avg → 0 → RSI → 100
        for i in 0..20 {
            rsi.update(100.0 + i as f64);
        }
        let v = rsi.get().unwrap();
        assert!(v > 95.0, "Expected RSI near 100, got {v}");
    }

    #[test]
    fn test_rsi_all_losses() {
        let mut rsi = RsiState::new(3);
        for i in 0..20 {
            rsi.update(100.0 - i as f64);
        }
        let v = rsi.get().unwrap();
        assert!(v < 5.0, "Expected RSI near 0, got {v}");
    }
}
