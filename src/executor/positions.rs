use crate::signals::scorer::Direction;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use std::collections::HashMap;

// ── OpenPosition ──────────────────────────────────────────────────────────────

/// One live position on Polymarket.
#[derive(Debug, Clone)]
pub struct OpenPosition {
    /// Polymarket condition ID (bytes32 hex).
    pub condition_id: String,
    /// ERC-1155 token ID we hold (YES or NO token).
    pub token_id: String,
    /// Direction that triggered the entry.
    pub direction: Direction,
    /// CLOB limit price paid (0.01 precision).
    pub entry_price: Decimal,
    /// USDC committed to this position.
    pub size_usdc: Decimal,
    /// CLOB order ID returned by the API.
    pub order_id: String,
    /// Binance stream key that generated the signal (e.g. "btcusdt_5m").
    pub stream_key: String,
    /// Wall-clock time the order was submitted.
    pub entered_at: DateTime<Utc>,
    /// When the Polymarket binary market expires.
    pub market_expiry: DateTime<Utc>,
    /// Human-readable question for logs / dashboard.
    pub question: String,
}

// ── PositionTracker ───────────────────────────────────────────────────────────

/// In-memory registry of all open positions. Not persisted to disk; rebuilt on
/// restart (positions placed in a previous session must be recovered manually
/// or via the auto-claimer).
#[derive(Debug, Default)]
pub struct PositionTracker {
    positions: HashMap<String, OpenPosition>, // keyed by condition_id
    pub realized_pnl: Decimal,
    pub wins: u32,
    pub losses: u32,
}

impl PositionTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a newly entered position.
    pub fn add(&mut self, pos: OpenPosition) {
        self.positions.insert(pos.condition_id.clone(), pos);
    }

    /// Remove a position by condition_id. Returns the removed entry if present.
    pub fn remove(&mut self, condition_id: &str) -> Option<OpenPosition> {
        self.positions.remove(condition_id)
    }

    /// True if we already have an open position in this market.
    pub fn contains(&self, condition_id: &str) -> bool {
        self.positions.contains_key(condition_id)
    }

    /// Clone a single position by condition_id (avoids holding the lock).
    pub fn get_cloned(&self, condition_id: &str) -> Option<OpenPosition> {
        self.positions.get(condition_id).cloned()
    }

    /// Iterate over all open positions.
    pub fn all(&self) -> impl Iterator<Item = &OpenPosition> {
        self.positions.values()
    }

    /// All open positions whose stream_key matches the given key.
    pub fn for_stream<'a>(&'a self, stream_key: &str) -> Vec<&'a OpenPosition> {
        self.positions
            .values()
            .filter(|p| p.stream_key == stream_key)
            .collect()
    }

    /// Condition IDs of positions whose market has expired (expiry ≤ now).
    pub fn expired_condition_ids(&self) -> Vec<String> {
        let now = Utc::now();
        self.positions
            .values()
            .filter(|p| p.market_expiry <= now)
            .map(|p| p.condition_id.clone())
            .collect()
    }

    /// Total USDC currently deployed across all open positions.
    pub fn total_deployed(&self) -> Decimal {
        self.positions.values().map(|p| p.size_usdc).sum()
    }

    /// Count of open positions.
    pub fn len(&self) -> usize {
        self.positions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.positions.is_empty()
    }

    /// Record the outcome of a closed position for aggregate statistics.
    pub fn record_exit(&mut self, won: bool, pnl: Decimal) {
        self.realized_pnl += pnl;
        if won {
            self.wins += 1;
        } else {
            self.losses += 1;
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signals::scorer::Direction;
    use rust_decimal_macros::dec;

    fn make_pos(cid: &str, stream: &str, expiry: DateTime<Utc>) -> OpenPosition {
        OpenPosition {
            condition_id: cid.to_string(),
            token_id: "tok1".to_string(),
            direction: Direction::Long,
            entry_price: dec!(0.60),
            size_usdc: dec!(50),
            order_id: "ord1".to_string(),
            stream_key: stream.to_string(),
            entered_at: Utc::now(),
            market_expiry: expiry,
            question: "Test?".to_string(),
        }
    }

    #[test]
    fn test_add_and_contains() {
        let mut pt = PositionTracker::new();
        let pos = make_pos("cid1", "btcusdt_5m", Utc::now() + chrono::Duration::hours(1));
        pt.add(pos);
        assert!(pt.contains("cid1"));
        assert!(!pt.contains("cid2"));
    }

    #[test]
    fn test_total_deployed() {
        let mut pt = PositionTracker::new();
        pt.add(make_pos("cid1", "btcusdt_5m", Utc::now() + chrono::Duration::hours(1)));
        pt.add(make_pos("cid2", "ethusdt_5m", Utc::now() + chrono::Duration::hours(1)));
        assert_eq!(pt.total_deployed(), dec!(100)); // 50 + 50
    }

    #[test]
    fn test_expired_condition_ids() {
        let mut pt = PositionTracker::new();
        let past = Utc::now() - chrono::Duration::seconds(1);
        let future = Utc::now() + chrono::Duration::hours(1);
        pt.add(make_pos("expired", "btcusdt_5m", past));
        pt.add(make_pos("active", "btcusdt_5m", future));
        let expired = pt.expired_condition_ids();
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0], "expired");
    }

    #[test]
    fn test_record_exit() {
        let mut pt = PositionTracker::new();
        pt.record_exit(true, dec!(10));
        pt.record_exit(false, dec!(-5));
        assert_eq!(pt.wins, 1);
        assert_eq!(pt.losses, 1);
        assert_eq!(pt.realized_pnl, dec!(5));
    }
}
