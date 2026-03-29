use chrono::{DateTime, Timelike, Utc};

/// VWAP — Volume Weighted Average Price with daily session reset (UTC 00:00).
///
/// Weight: 0.5 — Directional bias role.
#[derive(Debug, Clone)]
pub struct VwapState {
    pub cum_pv: f64,   // cumulative price × volume
    pub cum_vol: f64,  // cumulative volume
    cum_pv2: f64,      // cumulative price² × volume (for variance)
    pub session_start: DateTime<Utc>,
    pub bar_count: u32,
}

impl VwapState {
    pub fn new() -> Self {
        Self {
            cum_pv: 0.0,
            cum_vol: 0.0,
            cum_pv2: 0.0,
            session_start: Utc::now(),
            bar_count: 0,
        }
    }

    /// Feed an OHLCV bar. Resets session at UTC 00:00 boundary.
    pub fn update(&mut self, high: f64, low: f64, close: f64, volume: f64, ts: DateTime<Utc>) {
        // Reset at daily session boundary (UTC midnight)
        if self.should_reset(ts) {
            self.reset(ts);
        }

        if volume < 1e-12 {
            return;
        }

        // Typical price = (H + L + C) / 3
        let typical = (high + low + close) / 3.0;
        self.cum_pv += typical * volume;
        self.cum_pv2 += typical * typical * volume;
        self.cum_vol += volume;
        self.bar_count += 1;
    }

    fn should_reset(&self, ts: DateTime<Utc>) -> bool {
        // New UTC day started since session_start
        ts.date_naive() != self.session_start.date_naive()
    }

    fn reset(&mut self, ts: DateTime<Utc>) {
        self.cum_pv = 0.0;
        self.cum_vol = 0.0;
        self.cum_pv2 = 0.0;
        self.session_start = ts.with_hour(0).unwrap_or(ts);
        self.bar_count = 0;
    }

    pub fn vwap(&self) -> f64 {
        if self.cum_vol < 1e-12 {
            return 0.0;
        }
        self.cum_pv / self.cum_vol
    }

    pub fn variance(&self) -> f64 {
        if self.cum_vol < 1e-12 {
            return 0.0;
        }
        let mean = self.vwap();
        (self.cum_pv2 / self.cum_vol) - mean * mean
    }

    pub fn stddev(&self) -> f64 {
        self.variance().max(0.0).sqrt()
    }

    pub fn upper_band(&self) -> f64 {
        self.vwap() + 1.5 * self.stddev()
    }

    pub fn lower_band(&self) -> f64 {
        self.vwap() - 1.5 * self.stddev()
    }

    /// Deviation of price from VWAP in percent.
    pub fn deviation_pct(&self, price: f64) -> f64 {
        let vwap = self.vwap();
        if vwap < 1e-12 {
            return 0.0;
        }
        (price - vwap) / vwap * 100.0
    }

    pub fn is_ready(&self) -> bool {
        self.bar_count >= 3
    }

    /// Score: +0.5 (above VWAP), -0.5 (below), 0 (not ready).
    pub fn score(&self, price: f64) -> f64 {
        if !self.is_ready() {
            return 0.0;
        }
        let vwap = self.vwap();
        if price > vwap {
            0.5
        } else if price < vwap {
            -0.5
        } else {
            0.0
        }
    }
}

impl Default for VwapState {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn ts(y: i32, m: u32, d: u32, h: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, h, 0, 0).unwrap()
    }

    #[test]
    fn test_vwap_basic() {
        let mut v = VwapState::new();
        // Single bar: typical = (11+9+10)/3 = 10, vol = 100
        v.update(11.0, 9.0, 10.0, 100.0, ts(2024, 1, 1, 10));
        assert!((v.vwap() - 10.0).abs() < 1e-9);
    }

    #[test]
    fn test_vwap_session_reset() {
        let mut v = VwapState::new();
        v.update(11.0, 9.0, 10.0, 100.0, ts(2024, 1, 1, 23));
        let before = v.vwap();
        assert!(before > 0.0);
        // Next bar is on a new day → resets
        v.update(20.0, 18.0, 19.0, 200.0, ts(2024, 1, 2, 1));
        // VWAP should now reflect only the new bar
        let after = v.vwap();
        assert!(
            (after - 19.0).abs() < 0.5,
            "Expected VWAP ~19, got {after}"
        );
    }
}
