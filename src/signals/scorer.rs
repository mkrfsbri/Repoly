use crate::indicators::{
    AtrState, EmaStack, MacdSignal, MacdState, ObvState, RsiState, StochState, VwapState,
    VolatilityRegime,
};

// ── Direction ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum Direction {
    Long,
    Short,
}

// ── ConfluenceScore ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct ConfluenceScore {
    pub rsi_score: f64,   // weight 1.0, range [-1, +1]
    pub macd_score: f64,  // weight 1.5, range [-1.5, +1.5]
    pub stoch_score: f64, // weight 1.5, range [-1.5, +1.5]
    pub ema_score: f64,   // weight 1.0, range [-0.5, +0.5] (structure gate)
    pub obv_score: f64,   // weight 1.0, range [-1, +1]
    pub vwap_score: f64,  // weight 0.5, range [-0.5, +0.5]
    pub total: f64,       // sum, max ±7.0
    pub atr_regime: Option<VolatilityRegime>,
    pub blocked: bool, // true if ATR gate or core trio fails
    pub block_reason: Option<String>,
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

        // ATR hard gate (must check first)
        let regime = atr.regime(price, atr_min_pct, atr_max_pct);
        s.atr_regime = Some(regime.clone());

        if regime == VolatilityRegime::Extreme {
            s.blocked = true;
            s.block_reason = Some(format!(
                "ATR extreme ({:.2}%)",
                atr.atr_pct(price).unwrap_or(0.0)
            ));
            return s;
        }

        if regime == VolatilityRegime::Low {
            s.blocked = true;
            s.block_reason = Some(format!(
                "ATR low ({:.2}%)",
                atr.atr_pct(price).unwrap_or(0.0)
            ));
            return s;
        }

        // RSI — context filter (weight 1.0)
        s.rsi_score = rsi.score();

        // MACD — momentum (weight 1.5)
        s.macd_score = macd_signal.score();

        // Stochastic — entry trigger (weight 1.5)
        s.stoch_score = stoch.score();

        // EMA — structure gate (weight 1.0, capped ±0.5)
        s.ema_score = ema.structure_score(price);

        // OBV — confirmation (weight 1.0)
        let price_dir: i8 = if s.rsi_score + s.macd_score > 0.0 {
            1
        } else {
            -1
        };
        s.obv_score = obv.score(price_dir);

        // VWAP — directional bias (weight 0.5)
        s.vwap_score = vwap.score(price);

        s.total = s.rsi_score + s.macd_score + s.stoch_score + s.ema_score + s.obv_score + s.vwap_score;
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

    pub fn is_entry_signal(&self, threshold: f64) -> bool {
        !self.blocked && self.total.abs() >= threshold
    }
}

/// Check that RSI, MACD, and Stochastic all point the same direction.
///
/// This is a mandatory pre-condition for any valid entry, regardless of total score.
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
}
