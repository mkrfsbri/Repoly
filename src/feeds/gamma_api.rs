use anyhow::Result;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::Deserialize;
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::time::{sleep, Duration};
use tracing::{debug, error, info, warn};

// ── Domain types ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct PolyMarket {
    pub condition_id: String,
    pub token_yes_id: String,
    pub token_no_id: String,
    pub question: String,
    pub expiry: DateTime<Utc>,
    pub current_price: Decimal,
    pub volume_24h: Decimal,
    pub underlying: Underlying,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Underlying {
    Btc,
    Eth,
    Other,
}

impl PolyMarket {
    pub fn minutes_to_expiry(&self) -> i64 {
        (self.expiry - Utc::now()).num_minutes()
    }
}

// ── Gamma API response structs ───────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct GammaMarket {
    #[serde(rename = "conditionId")]
    condition_id: Option<String>,
    #[serde(rename = "clobTokenIds")]
    clob_token_ids: Option<Vec<String>>,
    question: Option<String>,
    #[serde(rename = "endDate")]
    end_date: Option<String>,
    #[serde(rename = "volume24hr")]
    volume_24hr: Option<f64>,
    #[serde(rename = "outcomePrices")]
    outcome_prices: Option<Vec<String>>,
    active: Option<bool>,
    closed: Option<bool>,
}

// ── GammaClient ──────────────────────────────────────────────────────────────

pub type MarketMap = Arc<RwLock<Vec<PolyMarket>>>;

pub struct GammaClient {
    base_url: String,
    min_volume_24h: f64,
    max_resolution_minutes: i64,
    pub markets: MarketMap,
    http: reqwest::Client,
}

impl GammaClient {
    pub fn new(base_url: String, min_volume_24h: f64, max_resolution_minutes: i64) -> Self {
        Self {
            base_url,
            min_volume_24h,
            max_resolution_minutes,
            markets: Arc::new(RwLock::new(Vec::new())),
            http: reqwest::Client::new(),
        }
    }

    /// Fetch and filter markets once.
    pub async fn refresh(&self) -> Result<usize> {
        let url = format!(
            "{}/markets?active=true&closed=false&limit=200",
            self.base_url
        );
        debug!("Fetching Gamma markets: {url}");

        let raw: Vec<GammaMarket> = self.http.get(&url).send().await?.json().await?;

        let now = Utc::now();
        let mut filtered = Vec::new();

        for m in raw {
            let condition_id = match m.condition_id {
                Some(id) if !id.is_empty() => id,
                _ => continue,
            };
            let active = m.active.unwrap_or(false);
            let closed = m.closed.unwrap_or(true);
            if !active || closed {
                continue;
            }
            let question = m.question.unwrap_or_default();
            let underlying = detect_underlying(&question);
            if underlying == Underlying::Other {
                continue;
            }

            let volume_24h = m.volume_24hr.unwrap_or(0.0);
            if volume_24h < self.min_volume_24h {
                continue;
            }

            let end_date = match m.end_date {
                Some(d) if !d.is_empty() => d,
                _ => continue,
            };
            let expiry = match parse_expiry(&end_date) {
                Ok(t) => t,
                Err(_) => continue,
            };
            let minutes_left = (expiry - now).num_minutes();
            if minutes_left <= 0 || minutes_left > self.max_resolution_minutes {
                continue;
            }

            let tokens = m.clob_token_ids.unwrap_or_default();
            if tokens.len() < 2 {
                continue;
            }

            let current_price = m
                .outcome_prices
                .as_deref()
                .and_then(|p| p.first())
                .and_then(|s| Decimal::from_str(s).ok())
                .unwrap_or(Decimal::new(50, 2));

            filtered.push(PolyMarket {
                condition_id,
                token_yes_id: tokens[0].clone(),
                token_no_id: tokens[1].clone(),
                question,
                expiry,
                current_price,
                volume_24h: Decimal::from_str(&volume_24h.to_string())
                    .unwrap_or(Decimal::ZERO),
                underlying,
            });
        }

        let count = filtered.len();
        *self.markets.write().await = filtered;
        info!("Gamma: {count} active markets found");
        Ok(count)
    }

    /// Run discovery loop, refreshing every `interval_secs`.
    pub async fn run(self: Arc<Self>, interval_secs: u64) {
        loop {
            if let Err(e) = self.refresh().await {
                error!("Gamma API error: {e:#}");
            }
            sleep(Duration::from_secs(interval_secs)).await;
        }
    }

    /// Get a snapshot of current markets.
    pub async fn active_markets(&self) -> Vec<PolyMarket> {
        self.markets.read().await.clone()
    }
}

fn detect_underlying(question: &str) -> Underlying {
    let q = question.to_lowercase();
    if q.contains("bitcoin") || q.contains("btc") {
        Underlying::Btc
    } else if q.contains("ethereum") || q.contains("eth") {
        Underlying::Eth
    } else {
        Underlying::Other
    }
}

fn parse_expiry(s: &str) -> Result<DateTime<Utc>> {
    // Try RFC3339 first, then common Gamma formats
    if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        return Ok(t.with_timezone(&Utc));
    }
    if let Ok(t) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S") {
        return Ok(DateTime::<Utc>::from_naive_utc_and_offset(t, Utc));
    }
    anyhow::bail!("Cannot parse expiry: {s}")
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_underlying() {
        assert_eq!(
            detect_underlying("Will Bitcoin exceed $70k?"),
            Underlying::Btc
        );
        assert_eq!(
            detect_underlying("ETH price above 4000 on Dec 31?"),
            Underlying::Eth
        );
        assert_eq!(detect_underlying("Will it rain tomorrow?"), Underlying::Other);
    }
}
