/// Exponential Moving Average — incremental O(1) update.
///
/// Uses standard EMA formula: EMA_t = α · price + (1 − α) · EMA_{t-1}
/// where α = 2 / (period + 1).
#[derive(Debug, Clone)]
pub struct EmaState {
    pub period: usize,
    pub value: f64,
    pub alpha: f64,
    pub initialized: bool,
    samples: usize,
    sum: f64, // used during SMA seed phase
}

impl EmaState {
    pub fn new(period: usize) -> Self {
        assert!(period > 0, "EMA period must be > 0");
        Self {
            period,
            value: 0.0,
            alpha: 2.0 / (period as f64 + 1.0),
            initialized: false,
            samples: 0,
            sum: 0.0,
        }
    }

    /// Update with a new price. Returns `Some(ema)` once warm-up is complete.
    pub fn update(&mut self, price: f64) -> Option<f64> {
        self.samples += 1;
        if !self.initialized {
            self.sum += price;
            if self.samples >= self.period {
                self.value = self.sum / self.period as f64;
                self.initialized = true;
                return Some(self.value);
            }
            return None;
        }
        self.value = self.alpha * price + (1.0 - self.alpha) * self.value;
        Some(self.value)
    }

    pub fn get(&self) -> Option<f64> {
        if self.initialized {
            Some(self.value)
        } else {
            None
        }
    }
}

// ── Triple EMA stack (fast / slow / filter) ──────────────────────────────────

#[derive(Debug, Clone)]
pub struct EmaStack {
    pub fast: EmaState,   // e.g. 9
    pub slow: EmaState,   // e.g. 21
    pub filter: EmaState, // e.g. 50
}

impl EmaStack {
    pub fn new(fast: usize, slow: usize, filter: usize) -> Self {
        Self {
            fast: EmaState::new(fast),
            slow: EmaState::new(slow),
            filter: EmaState::new(filter),
        }
    }

    pub fn update(&mut self, price: f64) {
        self.fast.update(price);
        self.slow.update(price);
        self.filter.update(price);
    }

    pub fn is_initialized(&self) -> bool {
        self.fast.initialized && self.slow.initialized && self.filter.initialized
    }

    /// Graduated structure score using full EMA stack alignment.
    ///
    /// +0.5  fast > slow > filter (clean bull structure)
    /// -0.5  fast < slow < filter (clean bear structure)
    /// +0.25 mixed EMAs but price above filter (soft bull bias)
    /// -0.25 mixed EMAs but price below filter (soft bear bias)
    ///  0.0  not yet initialized
    pub fn structure_score(&self, price: f64) -> f64 {
        if !self.is_initialized() {
            return 0.0;
        }
        let dir = self.direction(); // +1.0 bull, -1.0 bear, 0.0 choppy/mixed
        if dir != 0.0 {
            return dir * 0.5;
        }
        // Mixed alignment: softer signal from price position relative to filter EMA
        if price > self.filter.value { 0.25 } else { -0.25 }
    }

    /// True when fast > slow > filter and spread is meaningful.
    pub fn is_trending(&self, threshold_pct: f64) -> bool {
        if !self.is_initialized() {
            return false;
        }
        let spread = (self.fast.value - self.slow.value).abs();
        let spread_pct = spread / self.slow.value * 100.0;
        let direction_aligned = (self.fast.value - self.slow.value).signum()
            == (self.slow.value - self.filter.value).signum();
        spread_pct > threshold_pct && direction_aligned
    }

    pub fn is_choppy(&self) -> bool {
        !self.is_trending(0.05) // 0.05% threshold
    }

    /// +1.0 if bullish (fast > slow > filter), −1.0 if bearish, 0.0 if choppy.
    pub fn direction(&self) -> f64 {
        if !self.is_initialized() {
            return 0.0;
        }
        if self.fast.value > self.slow.value && self.slow.value > self.filter.value {
            1.0
        } else if self.fast.value < self.slow.value && self.slow.value < self.filter.value {
            -1.0
        } else {
            0.0
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ema_seed_phase() {
        let mut ema = EmaState::new(3);
        assert!(ema.update(10.0).is_none());
        assert!(ema.update(20.0).is_none());
        let v = ema.update(30.0).unwrap();
        // First EMA = SMA(3) = 20
        assert!((v - 20.0).abs() < 1e-9);
    }

    #[test]
    fn test_ema_subsequent_update() {
        let mut ema = EmaState::new(3);
        ema.update(10.0);
        ema.update(10.0);
        ema.update(10.0); // seed = 10
        let v = ema.update(20.0).unwrap();
        // α = 0.5,  EMA = 0.5*20 + 0.5*10 = 15
        assert!((v - 15.0).abs() < 1e-9);
    }
}
