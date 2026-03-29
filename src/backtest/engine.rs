use crate::config::{RiskConfig, SignalConfig, VolatilityConfig};
use crate::feeds::KlineBar;
use crate::indicators::IndicatorBundle;
use crate::risk::kelly::KellySizer;
use crate::signals::scorer::{ConfluenceScore, Direction};
use anyhow::Result;
use chrono::{DateTime, Utc};
use rayon::prelude::*;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};
use std::str::FromStr;
use tracing::{debug, info};

// ── CSV row structure from Binance Vision ────────────────────────────────────

#[derive(Debug, Deserialize)]
struct CsvKline {
    open_time: i64,
    open: f64,
    high: f64,
    low: f64,
    close: f64,
    volume: f64,
    close_time: i64,
    quote_asset_volume: f64,
    number_of_trades: u64,
    taker_buy_base: f64,
    taker_buy_quote: f64,
    ignore: f64,
}

// ── Trade record ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct TradeRecord {
    pub entry_bar: usize,
    pub exit_bar: usize,
    pub direction: String,
    pub entry_price: f64,
    pub exit_price: f64,
    pub size_usdc: f64,
    pub pnl: f64,
    pub won: bool,
    pub holding_bars: usize,
    pub entry_score: f64,
    pub entry_time: Option<i64>,
    pub exit_time: Option<i64>,
}

// ── Backtest result ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct BacktestResult {
    pub total_trades: u32,
    pub win_count: u32,
    pub loss_count: u32,
    pub win_rate: f64,
    pub total_pnl: f64,
    pub max_drawdown: f64,
    pub sharpe_ratio: f64,
    pub calmar_ratio: f64,
    pub avg_holding_bars: f64,
    pub final_balance: f64,
    pub trades: Vec<TradeRecord>,
    pub params: BacktestParams,
}

impl BacktestResult {
    pub fn combined_metric(&self) -> f64 {
        self.sharpe_ratio * self.calmar_ratio.max(0.01)
    }
}

// ── Configurable parameters for grid search ───────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BacktestParams {
    pub rsi_period: usize,
    pub atr_min_pct: f64,
    pub atr_max_pct: f64,
    pub confluence_threshold: f64,
    pub kelly_fraction: f64,
    /// Minimum bars between consecutive entry signals (was incorrectly using
    /// confluence_threshold as a bar count, which is a type confusion bug).
    pub cooldown_bars: usize,
}

impl Default for BacktestParams {
    fn default() -> Self {
        Self {
            rsi_period: 14,
            atr_min_pct: 0.15,
            atr_max_pct: 0.80,
            confluence_threshold: 3.5,
            kelly_fraction: 0.50,
            cooldown_bars: 2,
        }
    }
}

// ── BacktestConfig ────────────────────────────────────────────────────────────

pub struct BacktestConfig {
    pub initial_balance: f64,
    pub taker_fee_pct: f64, // 0.03 → 3% taker fee
    pub maker_rebate_pct: f64,
    pub slippage_ticks: u32,
    pub tick_size: f64,
    pub max_hold_bars: usize,
    pub params: BacktestParams,
}

impl Default for BacktestConfig {
    fn default() -> Self {
        Self {
            initial_balance: 1000.0,
            taker_fee_pct: 0.02,   // 2% taker
            maker_rebate_pct: 0.0025, // 0.25% maker rebate
            slippage_ticks: 1,
            tick_size: 0.01,
            max_hold_bars: 20,
            params: BacktestParams::default(),
        }
    }
}

// ── BacktestEngine ────────────────────────────────────────────────────────────

pub struct BacktestEngine {
    config: BacktestConfig,
}

impl BacktestEngine {
    pub fn new(config: BacktestConfig) -> Self {
        Self { config }
    }

    /// Load kline CSV from Binance Vision format.
    pub fn load_csv(path: &str) -> Result<Vec<KlineBar>> {
        let mut rdr = csv::ReaderBuilder::new()
            .has_headers(false)
            .from_path(path)?;
        let mut bars = Vec::new();

        for result in rdr.deserialize() {
            let row: CsvKline = result?;
            bars.push(KlineBar {
                timestamp: row.open_time,
                open: Decimal::from_str(&row.open.to_string())?,
                high: Decimal::from_str(&row.high.to_string())?,
                low: Decimal::from_str(&row.low.to_string())?,
                close: Decimal::from_str(&row.close.to_string())?,
                volume: Decimal::from_str(&row.volume.to_string())?,
                is_closed: true,
            });
        }
        Ok(bars)
    }

    /// Run a single backtest pass over a slice of bars.
    pub fn run(&self, bars: &[KlineBar]) -> BacktestResult {
        let params = &self.config.params;
        let mut indicators = IndicatorBundle::new();
        let mut balance = self.config.initial_balance;
        let mut peak_balance = balance;
        let mut max_drawdown = 0.0_f64;
        let mut trades: Vec<TradeRecord> = Vec::new();
        let mut kelly = KellySizer::new(
            50,
            Decimal::new(10, 0),
            Decimal::new(200, 0),
            params.kelly_fraction,
            0.05,
        );

        // Position state
        let mut in_position = false;
        let mut entry_bar = 0usize;
        let mut entry_price = 0.0_f64;
        let mut position_dir = Direction::Long;
        let mut position_size = 0.0_f64;
        let mut entry_score = 0.0_f64;
        let mut bars_since_entry = 0u32;
        let mut last_macd = crate::indicators::MacdSignal::Neutral;
        let mut last_signal_bar = 0usize;

        for (i, bar) in bars.iter().enumerate() {
            let close = bar.close.to_string().parse::<f64>().unwrap_or(0.0);
            let high = bar.high.to_string().parse::<f64>().unwrap_or(0.0);
            let low = bar.low.to_string().parse::<f64>().unwrap_or(0.0);
            let volume = bar.volume.to_string().parse::<f64>().unwrap_or(0.0);

            // Update all indicators
            indicators.rsi.update(close);
            indicators.ema.update(close);
            let macd_sig = indicators
                .macd
                .update(close)
                .unwrap_or(crate::indicators::MacdSignal::Neutral);
            last_macd = macd_sig.clone();
            indicators.stoch.update(high, low, close);
            indicators.obv.update(close, volume);
            indicators.vwap.update(
                high,
                low,
                close,
                volume,
                chrono::DateTime::from_timestamp_millis(bar.timestamp).unwrap_or(Utc::now()),
            );
            indicators.atr.update(high, low, close);

            if !indicators.is_ready() {
                continue;
            }

            if in_position {
                bars_since_entry += 1;

                // Exit after max_hold_bars
                if bars_since_entry as usize >= self.config.max_hold_bars {
                    let (pnl, won) = self.calculate_pnl(
                        &position_dir,
                        entry_price,
                        close,
                        position_size,
                    );
                    // Return deployed capital AND net profit/loss.
                    // (balance -= size was recorded at entry; we must add it back.)
                    balance += position_size + pnl;
                    kelly.record_trade(won, pnl);

                    let dd = (peak_balance - balance) / peak_balance;
                    max_drawdown = max_drawdown.max(dd);
                    if balance > peak_balance {
                        peak_balance = balance;
                    }

                    trades.push(TradeRecord {
                        entry_bar,
                        exit_bar: i,
                        direction: format!("{:?}", position_dir),
                        entry_price,
                        exit_price: close,
                        size_usdc: position_size,
                        pnl,
                        won,
                        holding_bars: bars_since_entry as usize,
                        entry_score,
                        entry_time: Some(bars[entry_bar].timestamp),
                        exit_time: Some(bar.timestamp),
                    });
                    in_position = false;
                    bars_since_entry = 0;
                }
                continue;
            }

            // Cooldown between signals (use dedicated bar count, NOT the score threshold)
            if i - last_signal_bar < params.cooldown_bars {
                continue;
            }

            // Compute confluence score
            let score = ConfluenceScore::compute(
                close,
                &indicators.rsi,
                &last_macd,
                &indicators.stoch,
                &indicators.ema,
                &indicators.obv,
                &indicators.vwap,
                &indicators.atr,
                params.atr_min_pct,
                params.atr_max_pct,
            );

            let dir = match score.direction() {
                Some(d) => d,
                None => continue,
            };

            if score.blocked {
                continue;
            }

            // Size the bet
            let bal_dec = Decimal::from_str(&balance.to_string()).unwrap_or(dec!(0));
            let size_dec = kelly.calculate_size(bal_dec);
            let size = size_dec.to_string().parse::<f64>().unwrap_or(10.0);

            if size > balance {
                continue;
            }

            // Enter position
            in_position = true;
            entry_bar = i;
            entry_price = close;
            position_dir = dir;
            position_size = size;
            entry_score = score.total;
            bars_since_entry = 0;
            last_signal_bar = i;
            balance -= size; // capital deployed
        }

        // Close any open position at end of data
        if in_position && entry_bar < bars.len() {
            let last_close = bars
                .last()
                .map(|b| b.close.to_string().parse::<f64>().unwrap_or(entry_price))
                .unwrap_or(entry_price);
            let (pnl, won) = self.calculate_pnl(
                &position_dir,
                entry_price,
                last_close,
                position_size,
            );
            balance += position_size + pnl;
        }

        let trade_pnls: Vec<f64> = trades.iter().map(|t| t.pnl).collect();
        let sharpe = calculate_sharpe(&trade_pnls);
        let total_pnl = trades.iter().map(|t| t.pnl).sum();
        let win_count = trades.iter().filter(|t| t.won).count() as u32;
        let avg_hold = if trades.is_empty() {
            0.0
        } else {
            trades.iter().map(|t| t.holding_bars as f64).sum::<f64>() / trades.len() as f64
        };
        let calmar = if max_drawdown > 0.0 {
            (total_pnl / self.config.initial_balance) / max_drawdown
        } else {
            0.0
        };

        let total = trades.len() as u32;
        BacktestResult {
            total_trades: total,
            win_count,
            loss_count: total - win_count,
            win_rate: if total > 0 { win_count as f64 / total as f64 } else { 0.0 },
            total_pnl,
            max_drawdown,
            sharpe_ratio: sharpe,
            calmar_ratio: calmar,
            avg_holding_bars: avg_hold,
            final_balance: balance,
            trades,
            params: self.config.params.clone(),
        }
    }

    fn calculate_pnl(
        &self,
        dir: &Direction,
        entry: f64,
        exit: f64,
        size: f64,
    ) -> (f64, bool) {
        // Simulate binary market pnl: price moves from entry to exit
        // In polymarket YES/NO, profit = (exit - entry) * size / entry (approx)
        let price_change = match dir {
            Direction::Long => (exit - entry) / entry,
            Direction::Short => (entry - exit) / entry,
        };
        let gross = size * price_change;
        let fee = size * self.config.taker_fee_pct;
        let pnl = gross - fee;
        (pnl, pnl > 0.0)
    }

    /// Walk-forward validation: splits data into train/test windows.
    pub fn walk_forward(
        &self,
        bars: &[KlineBar],
        train_bars: usize,
        test_bars: usize,
    ) -> Vec<(BacktestResult, BacktestResult)> {
        let mut results = Vec::new();
        let step = test_bars;
        let mut start = 0;

        while start + train_bars + test_bars <= bars.len() {
            let train = &bars[start..start + train_bars];
            let test = &bars[start + train_bars..start + train_bars + test_bars];

            let train_result = self.run(train);
            let test_result = self.run(test);
            results.push((train_result, test_result));

            start += step;
        }
        results
    }
}

// ── Grid Search ───────────────────────────────────────────────────────────────

pub struct GridSearch {
    pub rsi_periods: Vec<usize>,
    pub atr_min_bounds: Vec<f64>,
    pub atr_max_bounds: Vec<f64>,
    pub confluence_thresholds: Vec<f64>,
    pub kelly_fractions: Vec<f64>,
}

impl Default for GridSearch {
    fn default() -> Self {
        Self {
            rsi_periods: vec![9, 11, 14, 21],
            atr_min_bounds: vec![0.10, 0.12, 0.15, 0.18],
            atr_max_bounds: vec![0.60, 0.70, 0.80, 1.00],
            confluence_thresholds: vec![3.0, 3.5, 4.0, 4.5, 5.0],
            kelly_fractions: vec![0.25, 0.33, 0.50, 0.75],
        }
    }
}

impl GridSearch {
    pub fn run_parallel(
        &self,
        bars: &[KlineBar],
        initial_balance: f64,
    ) -> Vec<BacktestResult> {
        // Build all parameter combinations
        let mut params_list: Vec<BacktestParams> = Vec::new();
        for &rsi in &self.rsi_periods {
            for &atr_min in &self.atr_min_bounds {
                for &atr_max in &self.atr_max_bounds {
                    if atr_max <= atr_min {
                        continue;
                    }
                    for &thresh in &self.confluence_thresholds {
                        for &kelly in &self.kelly_fractions {
                            params_list.push(BacktestParams {
                                rsi_period: rsi,
                                atr_min_pct: atr_min,
                                atr_max_pct: atr_max,
                                confluence_threshold: thresh,
                                kelly_fraction: kelly,
                                cooldown_bars: 2,
                            });
                        }
                    }
                }
            }
        }

        info!("Grid search: {} parameter combinations", params_list.len());

        // Run in parallel with rayon
        let bars_ref = bars;
        params_list
            .into_par_iter()
            .map(|params| {
                let config = BacktestConfig {
                    initial_balance,
                    params,
                    ..Default::default()
                };
                let engine = BacktestEngine::new(config);
                engine.run(bars_ref)
            })
            .collect()
    }

    /// Find best params by Sharpe × Calmar, with anti-overfitting filter.
    pub fn best_params(
        results: &[BacktestResult],
        min_oos_sharpe_ratio: f64,
    ) -> Option<&BacktestResult> {
        results
            .iter()
            .filter(|r| r.total_trades >= 10)
            .filter(|r| r.sharpe_ratio >= min_oos_sharpe_ratio)
            .max_by(|a, b| {
                a.combined_metric()
                    .partial_cmp(&b.combined_metric())
                    .unwrap()
            })
    }
}

// ── Statistics helpers ────────────────────────────────────────────────────────

fn calculate_sharpe(returns: &[f64]) -> f64 {
    if returns.len() < 2 {
        return 0.0;
    }
    let mean = returns.iter().sum::<f64>() / returns.len() as f64;
    let variance =
        returns.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / (returns.len() - 1) as f64;
    let std = variance.sqrt();
    if std < 1e-12 {
        return 0.0;
    }
    mean / std * (252.0_f64).sqrt() // annualised (252 trading periods)
}
