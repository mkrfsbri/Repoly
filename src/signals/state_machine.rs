use crate::signals::scorer::{ConfluenceScore, Direction};
use tracing::{debug, info};

/// Signal lifecycle state for one pair.
#[derive(Debug, Clone, PartialEq)]
pub enum SignalState {
    Idle,
    Watching,  // Indicators aligning, not yet triggered
    Triggered, // All conditions met, order ready to submit
    Entered,   // Order submitted / filled
    Exited,    // Position closed or market resolved
}

impl SignalState {
    pub fn label(&self) -> &'static str {
        match self {
            SignalState::Idle => "IDLE",
            SignalState::Watching => "WATCHING",
            SignalState::Triggered => "TRIGGERED",
            SignalState::Entered => "ENTERED",
            SignalState::Exited => "EXITED",
        }
    }
}

/// Per-market state machine. Enforces strict state transitions and cooldowns.
#[derive(Debug, Clone)]
pub struct SignalMachine {
    pub state: SignalState,
    pub pair: String,
    pub bars_since_entry: u32,
    pub bars_since_exit: u32,
    pub cooldown_bars: u32,
    pub reentry_cooldown_bars: u32,
    pub last_score: Option<ConfluenceScore>,
    pub last_direction: Option<Direction>,
    /// How many bars the current score has been "watching" (partial alignment)
    watch_bars: u32,
}

impl SignalMachine {
    pub fn new(pair: &str, cooldown_bars: u32, reentry_cooldown_bars: u32) -> Self {
        Self {
            state: SignalState::Idle,
            pair: pair.to_string(),
            bars_since_entry: 0,
            bars_since_exit: 0,
            cooldown_bars,
            reentry_cooldown_bars,
            last_score: None,
            last_direction: None,
            watch_bars: 0,
        }
    }

    /// Tick the machine with a new ConfluenceScore. Returns the new state.
    pub fn tick(&mut self, score: ConfluenceScore, confluence_threshold: f64) -> &SignalState {
        // Increment counters
        if matches!(self.state, SignalState::Entered) {
            self.bars_since_entry += 1;
        }
        if matches!(self.state, SignalState::Exited) {
            self.bars_since_exit += 1;
        }

        let direction = score.direction();
        self.last_score = Some(score.clone());

        // Infer direction from score sign (works at any magnitude)
        let inferred_dir = if score.total > 0.0 {
            Some(Direction::Long)
        } else if score.total < 0.0 {
            Some(Direction::Short)
        } else {
            None
        };

        match &self.state {
            SignalState::Idle | SignalState::Exited => {
                // Respect cooldown after exit
                if matches!(self.state, SignalState::Exited)
                    && self.bars_since_exit < self.reentry_cooldown_bars
                {
                    return &self.state;
                }

                if score.blocked {
                    debug!(pair = %self.pair, reason = ?score.block_reason, "Entry blocked");
                    return &self.state;
                }

                // Partial alignment (≥60% threshold) → enter Watching
                let partial = score.total.abs() >= confluence_threshold * 0.6;
                if partial && inferred_dir.is_some() {
                    self.state = SignalState::Watching;
                    self.watch_bars = 0;
                    self.last_direction = inferred_dir;
                    debug!(pair = %self.pair, score = score.total, "Watching");
                }
            }

            SignalState::Watching => {
                self.watch_bars += 1;

                if score.blocked {
                    debug!(pair = %self.pair, "Watching → Idle (ATR gate)");
                    self.state = SignalState::Idle;
                    return &self.state;
                }

                // Direction flip: reset
                if inferred_dir != self.last_direction {
                    self.state = SignalState::Idle;
                    self.last_direction = None;
                    return &self.state;
                }

                if score.is_entry_signal(confluence_threshold) {
                    self.state = SignalState::Triggered;
                    self.last_direction = direction;
                    info!(
                        pair = %self.pair,
                        score = score.total,
                        direction = ?self.last_direction,
                        "TRIGGERED"
                    );
                } else if self.watch_bars > 5 {
                    // Stale — reset
                    self.state = SignalState::Idle;
                }
            }

            SignalState::Triggered => {
                // External code calls `mark_entered()` after order submission.
                // If we're still here after another tick, go back to Watching.
                self.state = SignalState::Watching;
            }

            SignalState::Entered => {
                // External code calls `mark_exited()` after position close.
            }
        }

        &self.state
    }

    /// Call after successfully submitting an order.
    /// No-ops silently if the state has already advanced past Triggered (e.g. the
    /// next bar arrived before the async order response came back).
    pub fn mark_entered(&mut self) {
        if self.state != SignalState::Triggered {
            return;
        }
        self.state = SignalState::Entered;
        self.bars_since_entry = 0;
        info!(pair = %self.pair, "ENTERED");
    }

    /// Call after position is closed or market resolves.
    pub fn mark_exited(&mut self) {
        self.state = SignalState::Exited;
        self.bars_since_exit = 0;
        info!(pair = %self.pair, "EXITED");
    }

    /// Reset to Idle (e.g. circuit breaker fires).
    pub fn reset(&mut self) {
        self.state = SignalState::Idle;
        self.last_direction = None;
        self.watch_bars = 0;
    }

    pub fn is_ready_to_enter(&self) -> bool {
        self.state == SignalState::Triggered
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indicators::VolatilityRegime;

    fn make_score(total: f64, blocked: bool) -> ConfluenceScore {
        ConfluenceScore {
            total,
            blocked,
            atr_regime: Some(VolatilityRegime::Optimal),
            ..Default::default()
        }
    }

    #[test]
    fn test_idle_to_watching() {
        let mut m = SignalMachine::new("BTC", 2, 5);
        let score = make_score(2.5, false); // partial alignment
        m.tick(score, 3.5);
        assert_eq!(m.state, SignalState::Watching);
    }

    #[test]
    fn test_watching_to_triggered() {
        let mut m = SignalMachine::new("BTC", 2, 5);
        m.tick(make_score(2.5, false), 3.5);
        m.tick(make_score(4.0, false), 3.5);
        assert_eq!(m.state, SignalState::Triggered);
    }

    #[test]
    fn test_atr_block_resets() {
        let mut m = SignalMachine::new("BTC", 2, 5);
        m.tick(make_score(2.5, false), 3.5);
        assert_eq!(m.state, SignalState::Watching);
        let mut blocked = make_score(5.0, true);
        blocked.block_reason = Some("ATR extreme".into());
        m.tick(blocked, 3.5);
        assert_eq!(m.state, SignalState::Idle);
    }
}
