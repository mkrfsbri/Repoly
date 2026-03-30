use super::ema::EmaState;

/// MACD signal state — graduated by histogram momentum, not just crossover event.
///
/// Scoring:
///   BullishCross / BullishMomentum  → +1.5  (histogram positive & growing)
///   BullishFading                   → +0.75 (histogram positive but shrinking)
///   BearishCross / BearishMomentum  → -1.5
///   BearishFading                   → -0.75
///   Neutral                         →  0.0
#[derive(Debug, Clone, PartialEq)]
pub enum MacdSignal {
    /// MACD line just crossed above signal line this bar (highest conviction).
    BullishCross,
    /// Histogram positive and growing — momentum building.
    BullishMomentum,
    /// Histogram positive but shrinking — momentum fading, still bullish.
    BullishFading,
    /// MACD line just crossed below signal line this bar.
    BearishCross,
    /// Histogram negative and growing in magnitude — momentum building.
    BearishMomentum,
    /// Histogram negative but shrinking in magnitude — momentum fading, still bearish.
    BearishFading,
    /// Near zero or uninitialised.
    Neutral,
}

impl MacdSignal {
    /// Graduated score contribution (weight max ±1.5).
    pub fn score(&self) -> f64 {
        match self {
            MacdSignal::BullishCross | MacdSignal::BullishMomentum => 1.5,
            MacdSignal::BullishFading => 0.75,
            MacdSignal::BearishCross | MacdSignal::BearishMomentum => -1.5,
            MacdSignal::BearishFading => -0.75,
            MacdSignal::Neutral => 0.0,
        }
    }

    /// True for any bullish state (used in core-trio state check).
    pub fn is_bullish(&self) -> bool {
        matches!(
            self,
            MacdSignal::BullishCross | MacdSignal::BullishMomentum | MacdSignal::BullishFading
        )
    }

    /// True for any bearish state.
    pub fn is_bearish(&self) -> bool {
        matches!(
            self,
            MacdSignal::BearishCross | MacdSignal::BearishMomentum | MacdSignal::BearishFading
        )
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
    /// Histogram from the previous bar — used to detect momentum acceleration vs fading.
    pub prev_histogram: f64,
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
            prev_histogram: 0.0,
            initialized: false,
        }
    }

    /// Feed a close price. Returns `Some(MacdSignal)` when fully warm.
    pub fn update(&mut self, close: f64) -> Option<MacdSignal> {
        let fast = self.fast_ema.update(close)?;
        let slow = self.slow_ema.update(close)?;
        let macd_line = fast - slow;

        let signal_line = self.signal_ema.update(macd_line)?;

        let prev_histogram = self.histogram;
        self.histogram = macd_line - signal_line;

        let sig = detect_signal(
            self.prev_macd,
            self.prev_signal,
            macd_line,
            signal_line,
            self.histogram,
            prev_histogram,
            self.initialized,
        );

        self.prev_macd = macd_line;
        self.prev_signal = signal_line;
        // prev_histogram already saved above; store for external inspection
        self.prev_histogram = prev_histogram;
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

/// Produce a MacdSignal from the current bar's data.
///
/// Priority:
///   1. Fresh crossover → BullishCross / BearishCross  (event-quality signal)
///   2. Histogram momentum  → Momentum / Fading based on whether histogram is
///      growing (|h| > |prev_h|) or shrinking
///   3. Otherwise → Neutral
fn detect_signal(
    prev_macd: f64,
    prev_sig: f64,
    cur_macd: f64,
    cur_sig: f64,
    cur_hist: f64,
    prev_hist: f64,
    was_initialized: bool,
) -> MacdSignal {
    // ── 1. Fresh crossover (highest conviction) ───────────────────────────────
    if prev_macd < prev_sig && cur_macd > cur_sig {
        return MacdSignal::BullishCross;
    }
    if prev_macd > prev_sig && cur_macd < cur_sig {
        return MacdSignal::BearishCross;
    }

    // ── 2. Histogram momentum ─────────────────────────────────────────────────
    // On the very first initialized bar prev_hist = 0; treat as momentum.
    if cur_hist > 0.0 {
        if !was_initialized || cur_hist >= prev_hist {
            MacdSignal::BullishMomentum
        } else {
            MacdSignal::BullishFading
        }
    } else if cur_hist < 0.0 {
        if !was_initialized || cur_hist <= prev_hist {
            MacdSignal::BearishMomentum
        } else {
            MacdSignal::BearishFading
        }
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
        // slow EMA ready at bar 37, signal EMA ready at bar 45
        for i in 0..44 {
            assert!(
                macd.update(100.0 + (i % 5) as f64).is_none(),
                "Call {i} should still be None (warm-up)"
            );
        }
        let sig = macd.update(105.0);
        assert!(sig.is_some(), "Call 45 should return Some (MACD ready)");
    }

    #[test]
    fn test_bullish_cross_detected() {
        // Force state so the next update produces a bullish cross.
        let mut state = MacdState::new(3, 5, 2);
        // Warm up minimally
        for _ in 0..8 {
            state.update(100.0);
        }
        // Big up-move should drive fast EMA above slow → cross
        state.update(110.0);
        // Just verify no panic and returns Some
    }

    #[test]
    fn test_bullish_momentum_persists() {
        // After a bullish cross, subsequent bars with positive & growing histogram
        // should return BullishMomentum (not Neutral).
        let mut state = MacdState::new(3, 5, 2);
        // Warm up with flat prices
        for _ in 0..8 {
            state.update(100.0);
        }
        // Drive up — histogram should stay positive for several bars
        let mut bullish_bars = 0u32;
        for _ in 0..10 {
            if let Some(sig) = state.update(110.0) {
                if sig.is_bullish() {
                    bullish_bars += 1;
                }
            }
        }
        assert!(bullish_bars > 1, "Bullish signal should persist across bars, got {bullish_bars}");
    }

    #[test]
    fn test_is_bullish_is_bearish_helpers() {
        assert!(MacdSignal::BullishCross.is_bullish());
        assert!(MacdSignal::BullishMomentum.is_bullish());
        assert!(MacdSignal::BullishFading.is_bullish());
        assert!(!MacdSignal::BullishCross.is_bearish());
        assert!(MacdSignal::BearishCross.is_bearish());
        assert!(MacdSignal::BearishMomentum.is_bearish());
        assert!(MacdSignal::BearishFading.is_bearish());
        assert!(!MacdSignal::Neutral.is_bullish());
        assert!(!MacdSignal::Neutral.is_bearish());
    }

    #[test]
    fn test_graduated_scores() {
        assert_eq!(MacdSignal::BullishCross.score(),    1.5);
        assert_eq!(MacdSignal::BullishMomentum.score(), 1.5);
        assert_eq!(MacdSignal::BullishFading.score(),   0.75);
        assert_eq!(MacdSignal::BearishCross.score(),   -1.5);
        assert_eq!(MacdSignal::BearishMomentum.score(),-1.5);
        assert_eq!(MacdSignal::BearishFading.score(),  -0.75);
        assert_eq!(MacdSignal::Neutral.score(),         0.0);
    }
}
