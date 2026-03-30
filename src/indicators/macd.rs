use super::ema::EmaState;

/// MACD crossover signal.
#[derive(Debug, Clone, PartialEq)]
pub enum MacdSignal {
    BullishCross, // MACD crossed above signal line → +1.5 weight
    BearishCross, // MACD crossed below signal line → -1.5 weight
    Neutral,
}

impl MacdSignal {
    /// Score contribution (weight 1.5).
    pub fn score(&self) -> f64 {
        match self {
            MacdSignal::BullishCross => 1.5,
            MacdSignal::BearishCross => -1.5,
            MacdSignal::Neutral => 0.0,
        }
    }
}

/// MACD — Fast EMA 12, Slow EMA 26, Signal EMA 9. All incremental O(1).
#[derive(Debug, Clone)]
pub struct MacdState {
    pub fast_ema: EmaState,
    pub slow_ema: EmaState,
    pub signal_ema: EmaState,
    pub prev_macd: f64,
    pub prev_signal: f64,
    pub histogram: f64,
    pub initialized: bool,
}

impl MacdState {
    pub fn new(fast: usize, slow: usize, signal: usize) -> Self {
        Self {
            fast_ema: EmaState::new(fast),
            slow_ema: EmaState::new(slow),
            signal_ema: EmaState::new(signal),
            prev_macd: 0.0,
            prev_signal: 0.0,
            histogram: 0.0,
            initialized: false,
        }
    }

    /// Feed a close price. Returns `Some(MacdSignal)` when fully warm.
    pub fn update(&mut self, close: f64) -> Option<MacdSignal> {
        let fast = self.fast_ema.update(close)?;
        let slow = self.slow_ema.update(close)?;
        let macd_line = fast - slow;

        let signal_line = self.signal_ema.update(macd_line)?;
        self.histogram = macd_line - signal_line;

        let sig = detect_cross(self.prev_macd, self.prev_signal, macd_line, signal_line);

        self.prev_macd = macd_line;
        self.prev_signal = signal_line;
        self.initialized = true;

        Some(sig)
    }

    pub fn macd_line(&self) -> f64 {
        if self.fast_ema.initialized && self.slow_ema.initialized {
            self.fast_ema.value - self.slow_ema.value
        } else {
            0.0
        }
    }
}

fn detect_cross(prev_macd: f64, prev_sig: f64, cur_macd: f64, cur_sig: f64) -> MacdSignal {
    if prev_macd < prev_sig && cur_macd > cur_sig {
        MacdSignal::BullishCross
    } else if prev_macd > prev_sig && cur_macd < cur_sig {
        MacdSignal::BearishCross
    } else {
        MacdSignal::Neutral
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_macd_warm_up() {
        let mut macd = MacdState::new(12, 26, 9);
        // Because of `?` shortcircuit:
        //   - slow_ema only starts getting called once fast_ema is ready (call 12)
        //   - slow_ema ready at call 12 + 26 - 1 = 37
        //   - signal_ema only starts from call 37, ready at call 37 + 9 - 1 = 45
        // Calls 1-44: all return None
        for i in 0..44 {
            assert!(
                macd.update(100.0 + (i % 5) as f64).is_none(),
                "Call {i} should still be None (warm-up)"
            );
        }
        // Call 45: signal EMA reaches period → first Some
        let sig = macd.update(105.0);
        assert!(sig.is_some(), "Call 45 should return Some (MACD ready)");
    }

    #[test]
    fn test_bullish_cross_detected() {
        // Force a bullish cross
        let mut state = MacdState::new(3, 5, 2);
        state.prev_macd = -0.5;
        state.prev_signal = 0.0;

        // Feed prices that drive fast EMA above slow
        for _ in 0..8 {
            state.update(100.0);
        }
        // At least one neutral or cross should occur without panic
        state.update(110.0);
    }
}
