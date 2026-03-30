use anyhow::Result;
use chrono::{DateTime, NaiveDate, Utc};
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

// ── Gamma API response structs ────────────────────────────────────────────────

/// Nested token object inside the `tokens` array.
#[derive(Debug, Deserialize)]
struct GammaToken {
    token_id: Option<String>,
    outcome: Option<String>,
    price: Option<serde_json::Value>, // may be f64 or "0.52" string
}

#[derive(Debug, Deserialize)]
struct GammaMarket {
    #[serde(rename = "conditionId")]
    condition_id: Option<String>,

    question: Option<String>,

    // Gamma may return token IDs as a flat string array...
    #[serde(rename = "clobTokenIds")]
    clob_token_ids: Option<Vec<String>>,
    // ...or as a nested object array.
    tokens: Option<Vec<GammaToken>>,

    #[serde(rename = "outcomePrices")]
    outcome_prices: Option<Vec<String>>,

    #[serde(rename = "endDate")]
    end_date: Option<String>,

    // BUG FIX: unwrap_or(false) caused every market with absent field to be silently dropped.
    // absent = assume active (we already filtered via ?active=true in the URL).
    #[serde(default = "bool_true")]
    active: bool,

    // BUG FIX: unwrap_or(true) caused every market with absent field to be dropped.
    // absent = assume not closed.
    #[serde(default = "bool_false")]
    closed: bool,

    // Skip archived markets.
    #[serde(default = "bool_false")]
    archived: bool,

    // BUG FIX: some markets only carry `volume`, not `volume24hr`.
    #[serde(rename = "volume24hr")]
    volume_24hr: Option<f64>,
    volume: Option<f64>,
}

fn bool_true() -> bool { true }
fn bool_false() -> bool { false }

// ── GammaClient ───────────────────────────────────────────────────────────────

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

    /// Fetch all pages and filter markets.
    pub async fn refresh(&self) -> Result<usize> {
        // BUG FIX: was limited to 200 results with no pagination.
        // Now fetches all pages until a short page is returned.
        const PAGE_LIMIT: usize = 200;
        let mut offset = 0usize;
        let mut all_raw: Vec<GammaMarket> = Vec::new();

        loop {
            let url = format!(
                "{}/markets?active=true&closed=false&limit={PAGE_LIMIT}&offset={offset}",
                self.base_url
            );
            debug!("Fetching Gamma markets: {url}");

            let page: Vec<GammaMarket> = match self.http.get(&url).send().await {
                Ok(resp) => match resp.json().await {
                    Ok(v) => v,
                    Err(e) => {
                        warn!("Gamma API JSON parse error at offset={offset}: {e}");
                        break;
                    }
                },
                Err(e) => {
                    warn!("Gamma API request failed at offset={offset}: {e}");
                    break;
                }
            };

            let page_len = page.len();
            all_raw.extend(page);
            debug!("Gamma page offset={offset}: {page_len} markets");

            if page_len < PAGE_LIMIT {
                break; // last page
            }
            offset += PAGE_LIMIT;
        }

        info!("Gamma: fetched {} total markets before filtering", all_raw.len());

        let now = Utc::now();
        let mut filtered = Vec::new();
        let mut rejected_active = 0usize;
        let mut rejected_question = 0usize;
        let mut rejected_volume = 0usize;
        let mut rejected_expiry = 0usize;
        let mut rejected_tokens = 0usize;

        for m in all_raw {
            let condition_id = match m.condition_id {
                Some(id) if !id.is_empty() => id,
                _ => continue,
            };

            // BUG FIX: was `m.active.unwrap_or(false)` and `m.closed.unwrap_or(true)`
            // which dropped ALL markets whenever these fields were absent in the JSON.
            // Now using serde default_fn so absent → true/false respectively.
            if !m.active || m.closed || m.archived {
                rejected_active += 1;
                debug!("Gamma rejected [active/closed/archived]: {condition_id}");
                continue;
            }

            let question = m.question.unwrap_or_default();
            let underlying = detect_underlying(&question);
            if underlying == Underlying::Other {
                rejected_question += 1;
                continue;
            }

            // BUG FIX: `volume24hr` absent → 0.0 < min_volume → filtered.
            // Use volume24hr first; fall back to generic `volume`.
            let effective_volume = m.volume_24hr
                .or(m.volume)
                .unwrap_or(0.0);
            if effective_volume < self.min_volume_24h {
                rejected_volume += 1;
                debug!(
                    "Gamma rejected [volume={effective_volume:.0} < {:.0}]: {condition_id} \"{}\"",
                    self.min_volume_24h,
                    &question[..question.len().min(60)]
                );
                continue;
            }

            let end_date = match m.end_date {
                Some(d) if !d.is_empty() => d,
                _ => {
                    rejected_expiry += 1;
                    continue;
                }
            };
            let expiry = match parse_expiry(&end_date) {
                Ok(t) => t,
                Err(e) => {
                    rejected_expiry += 1;
                    debug!("Gamma rejected [unparseable endDate={end_date:?}]: {e}");
                    continue;
                }
            };
            let minutes_left = (expiry - now).num_minutes();
            if minutes_left <= 0 || minutes_left > self.max_resolution_minutes {
                rejected_expiry += 1;
                debug!(
                    "Gamma rejected [minutes_left={minutes_left} not in 1..={}]: {condition_id}",
                    self.max_resolution_minutes
                );
                continue;
            }

            // BUG FIX: only tried `clobTokenIds` flat array. Now also falls back to
            // the nested `tokens[].token_id` array if clobTokenIds is absent/empty.
            let tokens = extract_token_ids(&m.clob_token_ids, &m.tokens);
            if tokens.len() < 2 {
                rejected_tokens += 1;
                debug!("Gamma rejected [token_count={}]: {condition_id}", tokens.len());
                continue;
            }

            let current_price = m
                .outcome_prices
                .as_deref()
                .and_then(|p| p.first())
                .and_then(|s| Decimal::from_str(s).ok())
                // Fallback: read price from nested tokens array
                .or_else(|| {
                    m.tokens.as_deref()?.first().and_then(|t| {
                        match &t.price {
                            Some(serde_json::Value::Number(n)) => {
                                Decimal::from_str(&n.to_string()).ok()
                            }
                            Some(serde_json::Value::String(s)) => Decimal::from_str(s).ok(),
                            _ => None,
                        }
                    })
                })
                .unwrap_or(Decimal::new(50, 2));

            filtered.push(PolyMarket {
                condition_id,
                token_yes_id: tokens[0].clone(),
                token_no_id: tokens[1].clone(),
                question,
                expiry,
                current_price,
                volume_24h: Decimal::from_str(&effective_volume.to_string())
                    .unwrap_or(Decimal::ZERO),
                underlying,
            });
        }

        let count = filtered.len();
        *self.markets.write().await = filtered;

        info!(
            "Gamma: {count} tradeable markets | rejected: active/closed={rejected_active} \
             question={rejected_question} volume={rejected_volume} \
             expiry={rejected_expiry} tokens={rejected_tokens}"
        );
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

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Try `clobTokenIds` first; fall back to `tokens[].token_id`.
fn extract_token_ids(
    flat: &Option<Vec<String>>,
    nested: &Option<Vec<GammaToken>>,
) -> Vec<String> {
    if let Some(ids) = flat {
        let non_empty: Vec<String> = ids.iter().filter(|s| !s.is_empty()).cloned().collect();
        if non_empty.len() >= 2 {
            return non_empty;
        }
    }
    if let Some(tok_list) = nested {
        let ids: Vec<String> = tok_list
            .iter()
            .filter_map(|t| t.token_id.as_deref().filter(|s| !s.is_empty()).map(str::to_string))
            .collect();
        return ids;
    }
    vec![]
}

pub fn detect_underlying(question: &str) -> Underlying {
    let q = question.to_lowercase();
    // BTC: accept "bitcoin", "btc", "btc-usd", "btcusd", "xbt"
    if q.contains("bitcoin") || q.contains(" btc") || q.starts_with("btc")
        || q.contains("btc-usd") || q.contains("btcusd") || q.contains(" xbt")
    {
        return Underlying::Btc;
    }
    // ETH: accept "ethereum", "eth", "eth-usd", "ethusd", "ether"
    if q.contains("ethereum") || q.contains(" eth") || q.starts_with("eth")
        || q.contains("eth-usd") || q.contains("ethusd") || q.contains("ether ")
    {
        return Underlying::Eth;
    }
    Underlying::Other
}

/// Parse a market expiry string into UTC DateTime.
///
/// Handles (in order):
/// 1. RFC 3339  "2025-03-30T10:05:00Z" / "…+00:00" / "…+05:30"
/// 2. Space-sep  "2025-03-30 10:05:00"
/// 3. No seconds "2025-03-30T10:05" / "2025-03-30 10:05"
/// 4. Date-only  "2025-03-30"  (treated as 23:59:59 UTC so it doesn't
///    expire at midnight and get dropped by the minutes_left > 0 check)
/// 5. Unix epoch (integer string) "1743330300"
pub fn parse_expiry(s: &str) -> Result<DateTime<Utc>> {
    let s = s.trim();

    // 1. RFC 3339 / ISO 8601 with timezone
    if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        return Ok(t.with_timezone(&Utc));
    }

    // 2. "YYYY-MM-DDTHH:MM:SS" (no tz → assume UTC)
    if let Ok(t) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S") {
        return Ok(t.and_utc());
    }

    // 3. Space separator "YYYY-MM-DD HH:MM:SS"
    if let Ok(t) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S") {
        return Ok(t.and_utc());
    }

    // 4. No seconds: "YYYY-MM-DDTHH:MM" or "YYYY-MM-DD HH:MM"
    if let Ok(t) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M") {
        return Ok(t.and_utc());
    }
    if let Ok(t) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M") {
        return Ok(t.and_utc());
    }

    // 5. Date-only "YYYY-MM-DD" → treat as end-of-day UTC
    if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        let t = d
            .and_hms_opt(23, 59, 59)
            .expect("23:59:59 is always valid");
        return Ok(t.and_utc());
    }

    // 6. Unix epoch integer (seconds)
    if let Ok(secs) = s.parse::<i64>() {
        if let Some(t) = DateTime::from_timestamp(secs, 0) {
            return Ok(t);
        }
    }

    anyhow::bail!("Cannot parse expiry: {s:?}")
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_underlying() {
        // Canonical cases
        assert_eq!(detect_underlying("Will Bitcoin exceed $70k?"), Underlying::Btc);
        assert_eq!(detect_underlying("ETH price above 4000 on Dec 31?"), Underlying::Eth);
        assert_eq!(detect_underlying("Will it rain tomorrow?"), Underlying::Other);

        // Polymarket BTC 5m/15m style questions
        assert_eq!(detect_underlying("BTC up or down in the next 5 minutes?"), Underlying::Btc);
        assert_eq!(detect_underlying("Will BTC-USD be higher at 10:05?"), Underlying::Btc);
        assert_eq!(detect_underlying("Crypto 15-min: BTC"), Underlying::Btc);

        // ETH short-term
        assert_eq!(detect_underlying("ETH-USD 5 min up/down"), Underlying::Eth);
        assert_eq!(detect_underlying("Ethereum price up in 15 min?"), Underlying::Eth);

        // Edge: avoid false positive on unrelated questions
        assert_eq!(detect_underlying("Will the batch process complete?"), Underlying::Other);
    }

    #[test]
    fn test_parse_expiry_rfc3339() {
        let t = parse_expiry("2025-03-30T10:05:00Z").unwrap();
        assert_eq!(t.format("%H:%M").to_string(), "10:05");
    }

    #[test]
    fn test_parse_expiry_rfc3339_millis() {
        // chrono's rfc3339 parser handles sub-second and explicit +00:00
        let t = parse_expiry("2025-03-30T10:05:00.000Z").unwrap();
        assert_eq!(t.format("%H:%M").to_string(), "10:05");
    }

    #[test]
    fn test_parse_expiry_space_sep() {
        let t = parse_expiry("2025-03-30 10:05:00").unwrap();
        assert_eq!(t.format("%H:%M").to_string(), "10:05");
    }

    #[test]
    fn test_parse_expiry_no_seconds() {
        let t = parse_expiry("2025-03-30T10:05").unwrap();
        assert_eq!(t.format("%H:%M").to_string(), "10:05");
    }

    #[test]
    fn test_parse_expiry_date_only() {
        let t = parse_expiry("2025-03-30").unwrap();
        // treated as end-of-day
        assert_eq!(t.format("%H:%M:%S").to_string(), "23:59:59");
    }

    #[test]
    fn test_parse_expiry_unix_timestamp() {
        // 2025-03-30 10:05:00 UTC
        let secs = DateTime::parse_from_rfc3339("2025-03-30T10:05:00Z")
            .unwrap()
            .timestamp();
        let t = parse_expiry(&secs.to_string()).unwrap();
        assert_eq!(t.format("%H:%M").to_string(), "10:05");
    }

    #[test]
    fn test_extract_token_ids_flat_preferred() {
        let flat = Some(vec!["tok_yes".to_string(), "tok_no".to_string()]);
        let nested = Some(vec![
            GammaToken { token_id: Some("other1".to_string()), outcome: None, price: None },
            GammaToken { token_id: Some("other2".to_string()), outcome: None, price: None },
        ]);
        let result = extract_token_ids(&flat, &nested);
        assert_eq!(result[0], "tok_yes");
    }

    #[test]
    fn test_extract_token_ids_falls_back_to_nested() {
        let flat: Option<Vec<String>> = None;
        let nested = Some(vec![
            GammaToken { token_id: Some("nested_yes".to_string()), outcome: None, price: None },
            GammaToken { token_id: Some("nested_no".to_string()), outcome: None, price: None },
        ]);
        let result = extract_token_ids(&flat, &nested);
        assert_eq!(result[0], "nested_yes");
        assert_eq!(result[1], "nested_no");
    }

    #[test]
    fn test_extract_token_ids_empty_flat_falls_back() {
        let flat = Some(vec!["".to_string(), "".to_string()]);
        let nested = Some(vec![
            GammaToken { token_id: Some("fallback_yes".to_string()), outcome: None, price: None },
            GammaToken { token_id: Some("fallback_no".to_string()), outcome: None, price: None },
        ]);
        let result = extract_token_ids(&flat, &nested);
        assert_eq!(result[0], "fallback_yes");
    }

    #[test]
    fn test_active_closed_defaults() {
        // Verify serde defaults: absent = active, absent = not closed
        let json = r#"{"conditionId":"0xabc","question":"BTC up in 5min?"}"#;
        let m: GammaMarket = serde_json::from_str(json).unwrap();
        assert!(m.active, "absent active field should default to true");
        assert!(!m.closed, "absent closed field should default to false");
        assert!(!m.archived, "absent archived field should default to false");
    }
}
