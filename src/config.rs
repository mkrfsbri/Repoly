use anyhow::{Context, Result};
use rust_decimal::Decimal;
use serde::Deserialize;
use std::str::FromStr;

/// Parse an f64 config value to Decimal without losing precision via an
/// uncontrolled to_string() call (which can produce "10.000000000000002" etc.).
fn f64_to_decimal(v: f64) -> Result<Decimal> {
    // Six decimal places is sufficient for USDC amounts; avoids f64 noise.
    Decimal::from_str(&format!("{v:.6}"))
        .map_err(|e| anyhow::anyhow!("Cannot convert {v} to Decimal: {e}"))
}

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
    pub claim: ClaimConfig,
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
    /// Cycle lengths in seconds (e.g. [300, 900] for 5-min and 15-min markets).
    pub interval_secs: Vec<i64>,
    /// Skip the current cycle and pre-fetch the next one when fewer than this
    /// many seconds remain in the current market window.
    pub min_entry_secs: i64,
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
    /// Parse tick_size string → Decimal. Errors surface at startup via Config::validate().
    pub fn tick_size_decimal(&self) -> Result<Decimal> {
        Decimal::from_str(&self.tick_size)
            .map_err(|e| anyhow::anyhow!("Invalid clob.tick_size '{}': {e}", self.tick_size))
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
    pub initial_balance_usdc: f64,
}

impl RiskConfig {
    pub fn min_bet(&self) -> Result<Decimal> {
        f64_to_decimal(self.min_bet_usdc)
            .with_context(|| format!("Invalid risk.min_bet_usdc: {}", self.min_bet_usdc))
    }

    pub fn max_bet(&self) -> Result<Decimal> {
        f64_to_decimal(self.max_bet_usdc)
            .with_context(|| format!("Invalid risk.max_bet_usdc: {}", self.max_bet_usdc))
    }

    pub fn initial_balance(&self) -> Result<Decimal> {
        f64_to_decimal(self.initial_balance_usdc)
            .with_context(|| format!("Invalid risk.initial_balance_usdc: {}", self.initial_balance_usdc))
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

#[derive(Debug, Clone, Deserialize)]
pub struct ClaimConfig {
    pub enabled: bool,
    pub check_interval_secs: u64,
    pub min_claimable_usdc: f64,
    pub use_relayer: bool,
    pub ctf_address: String,
    pub neg_risk_adapter: String,
}

impl Config {
    pub fn load(path: &str) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read config file: {path}"))?;
        let cfg: Self =
            toml::from_str(&content).with_context(|| "Failed to parse config.toml")?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Validate all fields that require non-trivial parsing or range checks.
    /// Called at startup — fail fast rather than hitting silent fallbacks at runtime.
    pub fn validate(&self) -> Result<()> {
        self.clob.tick_size_decimal()
            .context("clob.tick_size is invalid")?;
        self.risk.min_bet()
            .context("risk.min_bet_usdc is invalid")?;
        self.risk.max_bet()
            .context("risk.max_bet_usdc is invalid")?;

        anyhow::ensure!(
            self.risk.min_bet_usdc > 0.0,
            "risk.min_bet_usdc must be > 0"
        );
        anyhow::ensure!(
            self.risk.max_bet_usdc >= self.risk.min_bet_usdc,
            "risk.max_bet_usdc must be >= min_bet_usdc"
        );
        anyhow::ensure!(
            self.risk.kelly_fraction > 0.0 && self.risk.kelly_fraction <= 1.0,
            "risk.kelly_fraction must be in (0, 1]"
        );
        anyhow::ensure!(
            self.circuit_breaker.max_drawdown > 0.0 && self.circuit_breaker.max_drawdown < 1.0,
            "circuit_breaker.max_drawdown must be in (0, 1)"
        );
        anyhow::ensure!(
            self.volatility.atr_min_pct < self.volatility.atr_max_pct,
            "volatility.atr_min_pct must be < atr_max_pct"
        );
        Ok(())
    }
}
