use crate::signals::scorer::{ConfluenceScore, Direction};

/// A pending signal for one market.
#[derive(Debug, Clone)]
pub struct Signal {
    pub market_id: String,
    pub direction: Direction,
    pub score: ConfluenceScore,
    pub suggested_size_pct: f64, // 0..1, fraction of balance
}

/// Resolve conflicts across multiple simultaneous signals.
///
/// Rules:
/// 1. If two signals are opposing directions → keep the one with higher |total|.
/// 2. If all signals align → divide capital, each capped at `max_pct`.
/// 3. If more than `max_concurrent` signals → keep top N by score.
pub fn resolve_conflicts(
    signals: Vec<Signal>,
    max_concurrent: usize,
    max_single_pct: f64,  // e.g. 0.40
    max_deployed_pct: f64, // e.g. 0.80
) -> Vec<Signal> {
    if signals.is_empty() {
        return vec![];
    }

    // Separate by direction
    let mut longs: Vec<Signal> = signals
        .iter()
        .filter(|s| s.direction == Direction::Long)
        .cloned()
        .collect();
    let mut shorts: Vec<Signal> = signals
        .iter()
        .filter(|s| s.direction == Direction::Short)
        .cloned()
        .collect();

    // Sort each group by |score.total| descending
    longs.sort_by(|a, b| {
        b.score.total.abs().partial_cmp(&a.score.total.abs()).unwrap()
    });
    shorts.sort_by(|a, b| {
        b.score.total.abs().partial_cmp(&a.score.total.abs()).unwrap()
    });

    let mut result: Vec<Signal> = Vec::new();

    if !longs.is_empty() && !shorts.is_empty() {
        // Opposing signals: pick direction with highest score
        let best_long = &longs[0];
        let best_short = &shorts[0];
        if best_long.score.total.abs() >= best_short.score.total.abs() {
            result.push(longs.remove(0));
        } else {
            result.push(shorts.remove(0));
        }
    } else {
        // Same-direction: combine both groups
        result.extend(longs);
        result.extend(shorts);
    }

    // Limit to max_concurrent
    result.truncate(max_concurrent);

    // Allocate capital: split max_deployed_pct evenly, cap at max_single_pct
    let n = result.len() as f64;
    let per_signal = (max_deployed_pct / n).min(max_single_pct);

    for sig in &mut result {
        sig.suggested_size_pct = per_signal;
    }

    result
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indicators::VolatilityRegime;

    fn make_signal(id: &str, dir: Direction, total: f64) -> Signal {
        Signal {
            market_id: id.to_string(),
            direction: dir,
            score: ConfluenceScore {
                total,
                atr_regime: Some(VolatilityRegime::Optimal),
                ..Default::default()
            },
            suggested_size_pct: 0.0,
        }
    }

    #[test]
    fn test_opposing_picks_higher_score() {
        let signals = vec![
            make_signal("BTC-YES", Direction::Long, 4.5),
            make_signal("ETH-NO", Direction::Short, 3.6),
        ];
        let result = resolve_conflicts(signals, 3, 0.40, 0.80);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].direction, Direction::Long);
    }

    #[test]
    fn test_same_direction_allocates_evenly() {
        let signals = vec![
            make_signal("BTC-YES", Direction::Long, 4.5),
            make_signal("ETH-YES", Direction::Long, 3.8),
        ];
        let result = resolve_conflicts(signals, 3, 0.40, 0.80);
        assert_eq!(result.len(), 2);
        // 0.80 / 2 = 0.40, capped at 0.40
        assert!((result[0].suggested_size_pct - 0.40).abs() < 1e-9);
    }

    #[test]
    fn test_max_concurrent_limit() {
        let signals = (0..5)
            .map(|i| make_signal(&format!("MKT-{i}"), Direction::Long, 4.0 + i as f64 * 0.1))
            .collect();
        let result = resolve_conflicts(signals, 3, 0.40, 0.80);
        assert!(result.len() <= 3);
    }
}
