use crate::indicators::{
    AtrState, EmaStack, MacdSignal, ObvState, RsiState, StochState, VwapState,
    VolatilityRegime,
};

// ── Direction ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum Direction {
    Long,
    Short,
}

// ── ConfluenceScore ───────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ConfluenceScore {
    pub rsi_score: f64,    // weight 1.0, range [-1, +1]
    pub macd_score: f64,   // weight 1.5, range [-1.5, +1.5]
    pub stoch_score: f64,  // weight 1.5, range [-1.5, +1.5]
    pub ema_score: f64,    // weight 1.0, range [-0.5, +0.5] (structure gate)
    pub obv_score: f64,    // weight 1.0, range [-1, +1]
    pub vwap_score: f64,   // weight 0.5, range [-0.5, +0.5]
    pub total: f64,        // sum, max ±7.0
    pub atr_regime: Option<VolatilityRegime>,
    /// True when RSI + MACD + Stochastic all agree on direction.
    /// This is a MANDATORY pre-condition for entry, separate from `blocked`.
    pub core_trio_ok: bool,
    pub blocked: bool,
    pub block_reason: Option<String>,
}

impl Default for ConfluenceScore {
    fn default() -> Self {
        Self {
            rsi_score: 0.0,
            macd_score: 0.0,
            stoch_score: 0.0,
            ema_score: 0.0,
            obv_score: 0.0,
            vwap_score: 0.0,
            total: 0.0,
            atr_regime: None,
            // Default true so that test helpers using `..Default::default()` don't
            // accidentally block entries (the field is explicitly set during compute).
            core_trio_ok: true,
            blocked: false,
            block_reason: None,
        }
    }
}

impl ConfluenceScore {
    pub fn compute(
        price: f64,
        rsi: &RsiState,
        macd_signal: &MacdSignal,
        stoch: &StochState,
        ema: &EmaStack,
        obv: &ObvState,
        vwap: &VwapState,
        atr: &AtrState,
        atr_min_pct: f64,
        atr_max_pct: f64,
    ) -> Self {
        let mut s = Self::default();

        // ── ATR hard gate (checked before anything else) ──────────────────────
        let regime = atr.regime(price, atr_min_pct, atr_max_pct);
        s.atr_regime = Some(regime.clone());

        if regime == VolatilityRegime::Extreme {
            s.blocked = true;
            s.core_trio_ok = false;
            s.block_reason = Some(format!(
                "ATR extreme ({:.2}%)",
                atr.atr_pct(price).unwrap_or(0.0)
            ));
            return s;
        }

        if regime == VolatilityRegime::Low {
            s.blocked = true;
            s.core_trio_ok = false;
            s.block_reason = Some(format!(
                "ATR low ({:.2}%)",
                atr.atr_pct(price).unwrap_or(0.0)
            ));
            return s;
        }

        // ── Individual indicator scores ───────────────────────────────────────
        s.rsi_score   = rsi.score();
        s.macd_score  = macd_signal.score();
        s.stoch_score = stoch.score();
        s.ema_score   = ema.structure_score(price);

        let price_dir: i8 = if s.rsi_score + s.macd_score > 0.0 { 1 } else { -1 };
        s.obv_score  = obv.score(price_dir);
        s.vwap_score = vwap.score(price);

        s.total = s.rsi_score + s.macd_score + s.stoch_score
                + s.ema_score + s.obv_score  + s.vwap_score;

        // ── Core trio alignment check ─────────────────────────────────────────
        // RSI + MACD + Stochastic must ALL agree with the inferred direction.
        // This is a MANDATORY pre-condition for any valid entry.
        // Only apply when the total is strong enough to have a meaningful direction.
        let inferred_dir = if s.total > 0.0 { Direction::Long } else { Direction::Short };
        s.core_trio_ok = if s.total.abs() >= 1.5 {
            // Only enforce once we have a non-trivial signal
            core_trio_aligned(rsi, macd_signal, stoch, &inferred_dir)
        } else {
            // Weak / neutral score — don't block on core trio yet
            true
        };

        s
    }

    pub fn direction(&self) -> Option<Direction> {
        if self.total >= 3.5 {
            Some(Direction::Long)
        } else if self.total <= -3.5 {
            Some(Direction::Short)
        } else {
            None
        }
    }

    /// True when this score justifies submitting an entry order.
    ///
    /// Requires all three gates:
    /// 1. ATR regime is Optimal (not blocked)
    /// 2. Total confluence ≥ threshold
    /// 3. Core trio (RSI + MACD + Stochastic) all aligned
    pub fn is_entry_signal(&self, threshold: f64) -> bool {
        !self.blocked && self.total.abs() >= threshold && self.core_trio_ok
    }
}

/// Check that RSI, MACD, and Stochastic all agree on the same direction.
///
/// This is a mandatory pre-condition: even if the total confluence score is high,
/// if the core trio disagrees the signal is unreliable and must be blocked.
pub fn core_trio_aligned(
    rsi: &RsiState,
    macd: &MacdSignal,
    stoch: &StochState,
    direction: &Direction,
) -> bool {
    match direction {
        Direction::Long => {
            rsi.is_oversold()
                && *macd == MacdSignal::BullishCross
                && stoch.is_long_trigger()
        }
        Direction::Short => {
            rsi.is_overbought()
                && *macd == MacdSignal::BearishCross
                && stoch.is_short_trigger()
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_score_direction() {
        let mut s = ConfluenceScore::default();
        s.total = 4.0;
        assert_eq!(s.direction(), Some(Direction::Long));
        s.total = -4.0;
        assert_eq!(s.direction(), Some(Direction::Short));
        s.total = 2.0;
        assert_eq!(s.direction(), None);
    }

    #[test]
    fn test_is_entry_signal_requires_core_trio() {
        let mut s = ConfluenceScore::default();
        s.total = 4.5;
        s.core_trio_ok = false; // trio disagrees
        assert!(!s.is_entry_signal(3.5), "Should block when core trio fails");

        s.core_trio_ok = true;
        assert!(s.is_entry_signal(3.5), "Should pass when core trio aligns");
    }

    #[test]
    fn test_is_entry_signal_requires_not_blocked() {
        let mut s = ConfluenceScore::default();
        s.total = 5.0;
        s.core_trio_ok = true;
        s.blocked = true;
        assert!(!s.is_entry_signal(3.5), "ATR-blocked score must not enter");
    }
}
