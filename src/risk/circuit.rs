use rust_decimal::Decimal;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use tracing::{error, warn};

/// Thread-safe circuit breaker. Guards all execution paths.
///
/// Trips on:
/// - Drawdown from peak > `max_drawdown` (e.g. 15%)
/// - `max_consecutive_losses` consecutive losing trades
/// - API error rate > `max_error_rate` over recent window
#[derive(Debug)]
pub struct CircuitBreaker {
    pub is_open: AtomicBool,
    peak_balance: parking_lot::RwLock<Decimal>,
    consecutive_losses: AtomicU32,
    total_requests: AtomicU32,
    error_requests: AtomicU32,
    max_drawdown: f64,
    max_consecutive_losses: u32,
    max_error_rate: f64,
}

impl CircuitBreaker {
    pub fn new(
        initial_balance: Decimal,
        max_drawdown: f64,
        max_consecutive_losses: u32,
        max_error_rate: f64,
    ) -> Arc<Self> {
        Arc::new(Self {
            is_open: AtomicBool::new(false),
            peak_balance: parking_lot::RwLock::new(initial_balance),
            consecutive_losses: AtomicU32::new(0),
            total_requests: AtomicU32::new(0),
            error_requests: AtomicU32::new(0),
            max_drawdown,
            max_consecutive_losses,
            max_error_rate,
        })
    }

    /// Check balance-based conditions. Called after each balance refresh.
    pub fn check_balance(&self, current: Decimal) -> bool {
        // Acquire: see all stores that happened-before the trip() SeqCst store.
        if self.is_open.load(Ordering::Acquire) {
            return false;
        }
        let peak = *self.peak_balance.read();
        if peak.is_zero() {
            return true;
        }
        let drawdown = ((peak - current) / peak)
            .to_string()
            .parse::<f64>()
            .unwrap_or(0.0);

        if drawdown > self.max_drawdown {
            self.trip(format!("Drawdown {:.1}%", drawdown * 100.0));
            return false;
        }

        // Update peak
        if current > peak {
            *self.peak_balance.write() = current;
        }
        true
    }

    /// Record a trade result. Called after each completed trade.
    pub fn record_trade(&self, won: bool) {
        if won {
            self.consecutive_losses.store(0, Ordering::Relaxed);
        } else {
            let losses = self.consecutive_losses.fetch_add(1, Ordering::Relaxed) + 1;
            if losses >= self.max_consecutive_losses {
                self.trip(format!("{losses} consecutive losses"));
            }
        }
    }

    /// Record an API call result. Used for error-rate tracking.
    pub fn record_api_call(&self, success: bool) {
        self.total_requests.fetch_add(1, Ordering::Relaxed);
        if !success {
            let errors = self.error_requests.fetch_add(1, Ordering::Relaxed) + 1;
            let total = self.total_requests.load(Ordering::Relaxed);
            if total > 10 {
                let rate = errors as f64 / total as f64;
                if rate > self.max_error_rate {
                    self.trip(format!("API error rate {:.1}%", rate * 100.0));
                }
            }
        }
    }

    /// True if trading is allowed.
    pub fn is_ok(&self) -> bool {
        // Acquire matches the SeqCst Release in trip(), ensuring we never see
        // a stale false-OK state after a trip.
        !self.is_open.load(Ordering::Acquire)
    }

    /// Manually reset (e.g. after cooldown period or operator override).
    pub fn reset(&self) {
        self.is_open.store(false, Ordering::SeqCst);
        self.consecutive_losses.store(0, Ordering::Relaxed);
        self.error_requests.store(0, Ordering::Relaxed);
        self.total_requests.store(0, Ordering::Relaxed);
        warn!("Circuit breaker RESET");
    }

    fn trip(&self, reason: String) {
        self.is_open.store(true, Ordering::SeqCst);
        error!("🔴 CIRCUIT BREAKER OPEN: {reason}");
    }

    pub fn error_rate(&self) -> f64 {
        let total = self.total_requests.load(Ordering::Relaxed);
        if total == 0 {
            return 0.0;
        }
        self.error_requests.load(Ordering::Relaxed) as f64 / total as f64
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn test_drawdown_trips_breaker() {
        let cb = CircuitBreaker::new(dec!(1000), 0.15, 5, 0.20);
        cb.check_balance(dec!(1000)); // sets peak
        // 16% drawdown → check_balance trips the circuit and returns false
        let still_ok = cb.check_balance(dec!(840));
        assert!(!still_ok, "Expected false after exceeding drawdown limit");
        assert!(!cb.is_ok());
    }

    #[test]
    fn test_consecutive_losses() {
        let cb = CircuitBreaker::new(dec!(1000), 0.15, 3, 0.20);
        cb.record_trade(false);
        cb.record_trade(false);
        assert!(cb.is_ok());
        cb.record_trade(false); // 3rd loss → trips
        assert!(!cb.is_ok());
    }

    #[test]
    fn test_win_resets_streak() {
        let cb = CircuitBreaker::new(dec!(1000), 0.15, 3, 0.20);
        cb.record_trade(false);
        cb.record_trade(false);
        cb.record_trade(true); // resets streak
        cb.record_trade(false);
        cb.record_trade(false);
        assert!(cb.is_ok()); // only 2 consecutive losses
    }

    #[test]
    fn test_reset() {
        let cb = CircuitBreaker::new(dec!(1000), 0.15, 3, 0.20);
        cb.record_trade(false);
        cb.record_trade(false);
        cb.record_trade(false);
        assert!(!cb.is_ok());
        cb.reset();
        assert!(cb.is_ok());
    }
}
