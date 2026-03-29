pub mod atr;
pub mod ema;
pub mod macd;
pub mod obv;
pub mod rsi;
pub mod stochastic;
pub mod vwap;

pub use atr::{AtrState, VolatilityRegime};
pub use ema::{EmaStack, EmaState};
pub use macd::{MacdSignal, MacdState};
pub use obv::ObvState;
pub use rsi::RsiState;
pub use stochastic::StochState;
pub use vwap::VwapState;

/// Convenience bundle of all indicator states for one pair+interval.
pub struct IndicatorBundle {
    pub rsi: RsiState,
    pub ema: EmaStack,
    pub macd: MacdState,
    pub stoch: StochState,
    pub obv: ObvState,
    pub vwap: VwapState,
    pub atr: AtrState,
}

impl IndicatorBundle {
    pub fn new() -> Self {
        Self {
            rsi: RsiState::new(14),
            ema: EmaStack::new(9, 21, 50),
            macd: MacdState::new(12, 26, 9),
            stoch: StochState::new(14, 3, 3),
            obv: ObvState::new(20),
            vwap: VwapState::new(),
            atr: AtrState::new(14),
        }
    }

    pub fn is_ready(&self) -> bool {
        self.rsi.initialized && self.ema.is_initialized()
    }
}

impl Default for IndicatorBundle {
    fn default() -> Self {
        Self::new()
    }
}
