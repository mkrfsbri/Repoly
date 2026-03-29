use anyhow::{Context, Result};
use rust_decimal::Decimal;
use serde::Deserialize;
use std::str::FromStr;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub bot: BotConfig,
    pub binance: BinanceConfig,
    pub gamma: GammaConfig,
    pub polygon: PolygonConfig,
    pub clob: ClobConfig,
    pub risk: RiskConfig,
    pub circuit_breaker: CircuitBreakerConfig,
    pub volatility: VolatilityConfig,
    pub signal: SignalConfig,
    pub telegram: TelegramConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BotConfig {
    pub dry_run: bool,
    pub log_level: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BinanceConfig {
    pub ws_url: String,
    pub rest_url: String,
    pub kline_buffer_size: usize,
    pub streams: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GammaConfig {
    pub base_url: String,
    pub refresh_interval_secs: u64,
    pub min_volume_24h: f64,
    pub max_resolution_minutes: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PolygonConfig {
    pub rpc_url: String,
    pub usdc_address: String,
    pub balance_cache_secs: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ClobConfig {
    pub base_url: String,
    pub order_timeout_secs: u64,
    pub max_retries: u32,
    pub tick_size: String,
    pub maker_rebate_bps: u32,
}

impl ClobConfig {
    pub fn tick_size_decimal(&self) -> Decimal {
        Decimal::from_str(&self.tick_size).unwrap_or(Decimal::new(1, 2))
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct RiskConfig {
    pub max_position_pct: f64,
    pub max_deployed_pct: f64,
    pub min_bet_usdc: f64,
    pub max_bet_usdc: f64,
    pub kelly_fraction: f64,
    pub kelly_window: usize,
    pub cold_start_pct: f64,
}

impl RiskConfig {
    pub fn min_bet(&self) -> Decimal {
        Decimal::from_str(&self.min_bet_usdc.to_string()).unwrap_or(Decimal::new(10, 0))
    }

    pub fn max_bet(&self) -> Decimal {
        Decimal::from_str(&self.max_bet_usdc.to_string()).unwrap_or(Decimal::new(200, 0))
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct CircuitBreakerConfig {
    pub max_drawdown: f64,
    pub max_consecutive_losses: u32,
    pub max_api_error_rate: f64,
    pub cooldown_minutes: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct VolatilityConfig {
    pub atr_min_pct: f64,
    pub atr_max_pct: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SignalConfig {
    pub confluence_threshold: f64,
    pub cooldown_bars: u32,
    pub max_concurrent_positions: usize,
    pub reentry_cooldown_bars: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TelegramConfig {
    pub enabled: bool,
    pub bot_token: Option<String>,
    pub chat_id: Option<String>,
}

impl Config {
    pub fn load(path: &str) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read config file: {path}"))?;
        toml::from_str(&content).with_context(|| "Failed to parse config.toml")
    }
}
