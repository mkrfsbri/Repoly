use rust_decimal::Decimal;
use std::collections::VecDeque;
use std::str::FromStr;

/// Half-Kelly position sizer with a rolling trade window.
#[derive(Debug, Clone)]
pub struct KellySizer {
    window: usize,
    win_log: VecDeque<bool>,
    pnl_log: VecDeque<f64>,
    pub min_bet: Decimal,
    pub max_bet: Decimal,
    kelly_fraction: f64, // 0.5 for half-Kelly
    cold_start_pct: f64, // flat fraction used before window fills
    pub total_trades: u32,
    pub recalc_interval: u32, // recalculate every N trades
    cached_fraction: f64,
}

impl KellySizer {
    pub fn new(
        window: usize,
        min_bet: Decimal,
        max_bet: Decimal,
        kelly_fraction: f64,
        cold_start_pct: f64,
    ) -> Self {
        Self {
            window,
            win_log: VecDeque::with_capacity(window),
            pnl_log: VecDeque::with_capacity(window),
            min_bet,
            max_bet,
            kelly_fraction,
            cold_start_pct,
            total_trades: 0,
            recalc_interval: 10,
            cached_fraction: cold_start_pct,
        }
    }

    /// Record a completed trade.
    pub fn record_trade(&mut self, won: bool, pnl: f64) {
        if self.win_log.len() == self.window {
            self.win_log.pop_front();
            self.pnl_log.pop_front();
        }
        self.win_log.push_back(won);
        self.pnl_log.push_back(pnl);
        self.total_trades += 1;

        if self.total_trades % self.recalc_interval == 0 {
            self.cached_fraction = self.compute_fraction();
        }
    }

    /// Calculate bet size for the given balance.
    pub fn calculate_size(&self, balance: Decimal) -> Decimal {
        if self.win_log.len() < self.window / 2 {
            // Cold start: flat percentage
            let size = balance
                * Decimal::from_str(&self.cold_start_pct.to_string())
                    .unwrap_or(Decimal::new(5, 2));
            return size.clamp(self.min_bet, self.max_bet);
        }

        let fraction = self.cached_fraction;
        let fraction_dec = Decimal::from_str(&format!("{fraction:.6}"))
            .unwrap_or(Decimal::new(5, 2));

        let raw = balance * fraction_dec;
        raw.clamp(self.min_bet, self.max_bet)
    }

    fn compute_fraction(&self) -> f64 {
        if self.win_log.is_empty() {
            return self.cold_start_pct;
        }

        let wins: usize = self.win_log.iter().filter(|&&w| w).count();
        let p = wins as f64 / self.win_log.len() as f64;

        // Use the `won` boolean (not pnl sign) to categorise each trade.
        // This prevents divergence when a trade is recorded as won=true but
        // pnl is slightly negative (e.g. due to fees), or vice versa.
        let (win_sum, win_count, loss_sum, loss_count) = self.win_log.iter()
            .zip(self.pnl_log.iter())
            .fold(
                (0.0_f64, 0_usize, 0.0_f64, 0_usize),
                |(ws, wc, ls, lc), (&won, &pnl)| {
                    if won {
                        (ws + pnl.max(0.0), wc + 1, ls, lc)
                    } else {
                        (ws, wc, ls + pnl.abs(), lc + 1)
                    }
                },
            );

        let avg_win  = if win_count  > 0 { win_sum  / win_count  as f64 } else { 1.0 };
        let avg_loss = if loss_count > 0 { loss_sum / loss_count as f64 } else { 1.0 };

        let b = if avg_loss > 0.0 { avg_win / avg_loss } else { 1.0 };
        let full_kelly = (p * (b + 1.0) - 1.0) / b;
        let half_kelly = (full_kelly * self.kelly_fraction).max(0.0);

        half_kelly
    }

    pub fn win_rate(&self) -> f64 {
        if self.win_log.is_empty() {
            return 0.0;
        }
        let wins = self.win_log.iter().filter(|&&w| w).count();
        wins as f64 / self.win_log.len() as f64
    }

    pub fn avg_win_loss_ratio(&self) -> f64 {
        let wins: Vec<f64> = self.pnl_log.iter().filter(|&&p| p > 0.0).cloned().collect();
        let losses: Vec<f64> = self.pnl_log.iter().filter(|&&p| p < 0.0).cloned().collect();
        let avg_win = if wins.is_empty() { 0.0 } else { wins.iter().sum::<f64>() / wins.len() as f64 };
        let avg_loss = if losses.is_empty() { 0.0 } else { losses.iter().map(|x| x.abs()).sum::<f64>() / losses.len() as f64 };
        if avg_loss > 0.0 { avg_win / avg_loss } else { 0.0 }
    }

    pub fn is_warm(&self) -> bool {
        self.win_log.len() >= self.window / 2
    }

    /// Current Kelly fraction used for sizing (cold-start flat or live estimate).
    pub fn current_fraction(&self) -> f64 {
        self.cached_fraction
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn test_cold_start_flat_pct() {
        let sizer = KellySizer::new(50, dec!(10), dec!(200), 0.5, 0.05);
        let size = sizer.calculate_size(dec!(1000));
        // 5% of 1000 = 50, clamped to [10, 200]
        assert_eq!(size, dec!(50));
    }

    #[test]
    fn test_clamp_to_min() {
        let sizer = KellySizer::new(50, dec!(10), dec!(200), 0.5, 0.001);
        let size = sizer.calculate_size(dec!(100));
        assert_eq!(size, dec!(10));
    }

    #[test]
    fn test_clamp_to_max() {
        let sizer = KellySizer::new(50, dec!(10), dec!(200), 0.5, 1.0);
        let size = sizer.calculate_size(dec!(10000));
        assert_eq!(size, dec!(200));
    }

    #[test]
    fn test_win_rate() {
        let mut sizer = KellySizer::new(10, dec!(10), dec!(200), 0.5, 0.05);
        for i in 0..10 {
            sizer.record_trade(i % 2 == 0, if i % 2 == 0 { 10.0 } else { -5.0 });
        }
        let wr = sizer.win_rate();
        assert!((wr - 0.5).abs() < 0.01);
    }
}
