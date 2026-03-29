/// ATR — Average True Range, Wilder's smoothing. Incremental O(1).
///
/// Hard gate: not a scorer. `VolatilityRegime::Extreme` blocks all entries.
#[derive(Debug, Clone, PartialEq)]
pub enum VolatilityRegime {
    Low,     // ATR% < min threshold → market sleeping, no edge
    Optimal, // ATR% in [min, max] → tradeable
    Extreme, // ATR% > max threshold → chaos, block all entries
}

impl VolatilityRegime {
    pub fn is_tradeable(&self) -> bool {
        matches!(self, VolatilityRegime::Optimal)
    }

    pub fn label(&self) -> &'static str {
        match self {
            VolatilityRegime::Low => "low",
            VolatilityRegime::Optimal => "optimal",
            VolatilityRegime::Extreme => "extreme",
        }
    }
}

#[derive(Debug, Clone)]
pub struct AtrState {
    pub period: usize,
    pub value: f64,
    prev_close: f64,
    initialized: bool,
    samples: usize,
    seed_sum: f64,
}

impl AtrState {
    pub fn new(period: usize) -> Self {
        assert!(period > 0);
        Self {
            period,
            value: 0.0,
            prev_close: 0.0,
            initialized: false,
            samples: 0,
            seed_sum: 0.0,
        }
    }

    /// Feed HLC values. Returns `Some(atr)` once warm-up is complete.
    pub fn update(&mut self, high: f64, low: f64, close: f64) -> Option<f64> {
        let tr = if self.samples == 0 {
            // First bar: TR = High - Low (no prev close)
            high - low
        } else {
            true_range(high, low, self.prev_close)
        };

        self.prev_close = close;
        self.samples += 1;

        if !self.initialized {
            self.seed_sum += tr;
            if self.samples >= self.period {
                self.value = self.seed_sum / self.period as f64;
                self.initialized = true;
                return Some(self.value);
            }
            return None;
        }

        // Wilder's smoothing
        let n = self.period as f64;
        self.value = (self.value * (n - 1.0) + tr) / n;
        Some(self.value)
    }

    pub fn get(&self) -> Option<f64> {
        if self.initialized {
            Some(self.value)
        } else {
            None
        }
    }

    /// ATR as a percentage of price.
    pub fn atr_pct(&self, price: f64) -> Option<f64> {
        if price < 1e-12 {
            return None;
        }
        self.get().map(|atr| atr / price * 100.0)
    }

    /// Determine volatility regime using configurable thresholds.
    pub fn regime(&self, price: f64, min_pct: f64, max_pct: f64) -> VolatilityRegime {
        match self.atr_pct(price) {
            None => VolatilityRegime::Low,
            Some(pct) if pct < min_pct => VolatilityRegime::Low,
            Some(pct) if pct > max_pct => VolatilityRegime::Extreme,
            _ => VolatilityRegime::Optimal,
        }
    }
}

/// True Range: max(High−Low, |High−PrevClose|, |Low−PrevClose|)
fn true_range(high: f64, low: f64, prev_close: f64) -> f64 {
    let hl = high - low;
    let hc = (high - prev_close).abs();
    let lc = (low - prev_close).abs();
    hl.max(hc).max(lc)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_atr_warm_up() {
        let mut atr = AtrState::new(3);
        assert!(atr.update(105.0, 95.0, 100.0).is_none());
        assert!(atr.update(106.0, 96.0, 101.0).is_none());
        let v = atr.update(107.0, 97.0, 102.0).unwrap();
        assert!(v > 0.0);
    }

    #[test]
    fn test_true_range_with_gap() {
        // Gap up: prev_close=100, high=115, low=108
        // TR = max(7, 15, 8) = 15
        let tr = true_range(115.0, 108.0, 100.0);
        assert!((tr - 15.0).abs() < 1e-9);
    }

    #[test]
    fn test_regime_classification() {
        let mut atr = AtrState::new(3);
        for _ in 0..3 {
            atr.update(67500.0, 67000.0, 67250.0); // ~0.37% ATR
        }
        if let Some(v) = atr.get() {
            println!("ATR: {v}");
        }
        // 500/67250 ≈ 0.74% — should be Optimal with defaults [0.15, 0.80]
        let r = atr.regime(67250.0, 0.15, 0.80);
        assert_eq!(r, VolatilityRegime::Optimal);
    }
}
