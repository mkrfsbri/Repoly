use anyhow::Result;
use chrono::{DateTime, NaiveDate, Utc};
use rust_decimal::Decimal;
use serde::Deserialize;
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::time::{sleep, Duration};
use tracing::{debug, error, info, warn};

// ── Domain types ──────────────────────────────────────────────────────────────

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
    /// Cycle length in seconds (300 or 900).
    pub interval_secs: i64,
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

#[derive(Debug, Deserialize)]
struct GammaToken {
    token_id: Option<String>,
    outcome: Option<String>,
    price: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct GammaMarket {
    #[serde(rename = "conditionId")]
    condition_id: Option<String>,

    question: Option<String>,

    #[serde(rename = "clobTokenIds")]
    clob_token_ids: Option<Vec<String>>,
    tokens: Option<Vec<GammaToken>>,

    #[serde(rename = "outcomePrices")]
    outcome_prices: Option<Vec<String>>,

    #[serde(rename = "endDate")]
    end_date: Option<String>,

    #[serde(default = "bool_true")]
    active: bool,
    #[serde(default = "bool_false")]
    closed: bool,
    #[serde(default = "bool_false")]
    archived: bool,

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
    /// Cycle lengths we care about (seconds): [300, 900].
    interval_secs: Vec<i64>,
    /// When fewer than this many seconds remain in the current window,
    /// pre-fetch the NEXT cycle's market instead.
    min_entry_secs: i64,
    pub markets: MarketMap,
    http: reqwest::Client,
}

impl GammaClient {
    pub fn new(
        base_url: String,
        min_volume_24h: f64,
        interval_secs: Vec<i64>,
        min_entry_secs: i64,
    ) -> Self {
        Self {
            base_url,
            min_volume_24h,
            interval_secs,
            min_entry_secs,
            markets: Arc::new(RwLock::new(Vec::new())),
            http: reqwest::Client::new(),
        }
    }

    /// Refresh the market list using deterministic cycle-based queries.
    ///
    /// For each configured interval (e.g. 300 s, 900 s) we compute the target
    /// expiry timestamp for the currently active window (or the next one when
    /// fewer than `min_entry_secs` remain) and query the Gamma API with a
    /// ±30-second `end_date` window. This avoids full pagination and always
    /// hits the exact markets we intend to trade.
    pub async fn refresh(&self) -> Result<usize> {
        let now = Utc::now();
        // Collect filtered PolyMarkets per-interval so each market carries the
        // correct interval_secs tag.  We cannot determine this post-hoc because
        // 15-min boundaries are also 5-min boundaries (900 % 300 == 0).
        let mut all_markets: Vec<PolyMarket> = Vec::new();

        for &interval in &self.interval_secs {
            let target = cycle_target_expiry(interval, self.min_entry_secs, now);

            // ± 30 s tolerance window around the exact cycle boundary.
            let win_start = target - chrono::Duration::seconds(30);
            let win_end   = target + chrono::Duration::seconds(30);

            let url = format!(
                "{}/markets?active=true&closed=false\
                 &end_date_min={}&end_date_max={}&limit=50",
                self.base_url,
                win_start.format("%Y-%m-%dT%H:%M:%SZ"),
                win_end.format("%Y-%m-%dT%H:%M:%SZ"),
            );

            debug!(
                "Gamma cycle={}s target={} query: {}",
                interval,
                target.format("%H:%M:%S"),
                url
            );

            match self.fetch_raw(&url).await {
                Ok(page) => {
                    info!(
                        "Gamma cycle={}s expiry={}: {} raw markets returned",
                        interval,
                        target.format("%H:%M:%S"),
                        page.len()
                    );
                    all_markets.extend(self.filter_for_interval(page, now, interval));
                }
                Err(e) => {
                    warn!("Gamma cycle={}s query failed: {e}", interval);
                }
            }
        }

        // Deduplicate by condition_id (a market can appear in both 5-min and
        // 15-min queries when their boundaries coincide; keep the first, which
        // has the lower/more-specific interval).
        let mut seen = std::collections::HashSet::new();
        let filtered: Vec<PolyMarket> = all_markets
            .into_iter()
            .filter(|m| seen.insert(m.condition_id.clone()))
            .collect();

        let count = filtered.len();
        *self.markets.write().await = filtered;

        if count == 0 {
            warn!(
                "Gamma: 0 tradeable markets found. \
                 Intervals: {:?}  min_entry_secs: {}  now: {}",
                self.interval_secs,
                self.min_entry_secs,
                now.format("%H:%M:%S")
            );
        } else {
            info!("Gamma: {} tradeable market(s) ready", count);
        }

        Ok(count)
    }

    /// Run discovery loop, refreshing every `interval_secs`.
    pub async fn run(self: Arc<Self>, refresh_secs: u64) {
        loop {
            if let Err(e) = self.refresh().await {
                error!("Gamma API error: {e:#}");
            }
            sleep(Duration::from_secs(refresh_secs)).await;
        }
    }

    /// Snapshot of current tradeable markets.
    pub async fn active_markets(&self) -> Vec<PolyMarket> {
        self.markets.read().await.clone()
    }

    // ── Private helpers ───────────────────────────────────────────────────────

    async fn fetch_raw(&self, url: &str) -> Result<Vec<GammaMarket>> {
        let resp = self
            .http
            .get(url)
            .timeout(Duration::from_secs(10))
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Gamma HTTP {status}: {body}");
        }

        Ok(resp.json::<Vec<GammaMarket>>().await?)
    }

    fn filter_for_interval(&self, raw: Vec<GammaMarket>, now: DateTime<Utc>, interval: i64) -> Vec<PolyMarket> {
        let mut out = Vec::new();
        let mut rej_status  = 0usize;
        let mut rej_asset   = 0usize;
        let mut rej_volume  = 0usize;
        let mut rej_expiry  = 0usize;
        let mut rej_tokens  = 0usize;

        'market: for m in raw {
            let cid = match m.condition_id.as_deref() {
                Some(s) if !s.is_empty() => s.to_string(),
                _ => continue,
            };

            if !m.active || m.closed || m.archived {
                rej_status += 1;
                continue;
            }

            let question = m.question.clone().unwrap_or_default();
            let underlying = detect_underlying(&question);
            if underlying == Underlying::Other {
                rej_asset += 1;
                continue;
            }

            let vol = m.volume_24hr.or(m.volume).unwrap_or(0.0);
            if vol < self.min_volume_24h {
                rej_volume += 1;
                debug!(
                    "Gamma reject [vol={vol:.0}<{:.0}]: {cid}",
                    self.min_volume_24h
                );
                continue;
            }

            let end_str = match m.end_date.as_deref() {
                Some(s) if !s.is_empty() => s.to_string(),
                _ => { rej_expiry += 1; continue; }
            };
            let expiry = match parse_expiry(&end_str) {
                Ok(t) => t,
                Err(e) => {
                    rej_expiry += 1;
                    debug!("Gamma reject [bad end_date={end_str:?} {e}]: {cid}");
                    continue;
                }
            };
            if expiry <= now {
                rej_expiry += 1;
                debug!("Gamma reject [already expired]: {cid}");
                continue;
            }

            let token_ids = extract_token_ids(&m.clob_token_ids, &m.tokens);
            if token_ids.len() < 2 {
                rej_tokens += 1;
                debug!("Gamma reject [tokens={}]: {cid}", token_ids.len());
                continue;
            }

            // Use the interval we queried with — do NOT try to infer it from
            // the expiry timestamp because 15-min boundaries are also 5-min
            // boundaries (900 % 300 == 0).
            let interval_secs = interval;

            let current_price = m
                .outcome_prices
                .as_deref()
                .and_then(|p| p.first())
                .and_then(|s| Decimal::from_str(s).ok())
                .or_else(|| price_from_tokens(m.tokens.as_deref()))
                .unwrap_or(Decimal::new(50, 2));

            // Deduplicate by condition_id (cycle-based queries can return the
            // same market from overlapping windows).
            if out.iter().any(|p: &PolyMarket| p.condition_id == cid) {
                continue 'market;
            }

            out.push(PolyMarket {
                condition_id: cid,
                token_yes_id: token_ids[0].clone(),
                token_no_id:  token_ids[1].clone(),
                question,
                expiry,
                current_price,
                volume_24h: Decimal::from_str(&format!("{vol:.6}")).unwrap_or(Decimal::ZERO),
                underlying,
                interval_secs,
            });
        }

        debug!(
            "Gamma filter: ok={} rej_status={rej_status} rej_asset={rej_asset} \
             rej_vol={rej_volume} rej_expiry={rej_expiry} rej_tokens={rej_tokens}",
            out.len()
        );

        out
    }
}

// ── Cycle helpers ─────────────────────────────────────────────────────────────

/// Compute the target cycle expiry that we want to trade right now.
///
/// The "current" cycle boundary is `ceil(now / interval) * interval`.
/// If fewer than `min_entry_secs` remain in that window, return the NEXT
/// boundary so we pre-fetch the upcoming market while the current one winds
/// down.
pub fn cycle_target_expiry(
    interval_secs: i64,
    min_entry_secs: i64,
    now: DateTime<Utc>,
) -> DateTime<Utc> {
    let ts = now.timestamp();
    let current_boundary = ((ts / interval_secs) + 1) * interval_secs;
    let remaining = current_boundary - ts;

    let target_ts = if remaining < min_entry_secs {
        current_boundary + interval_secs // pre-fetch next cycle
    } else {
        current_boundary
    };

    DateTime::from_timestamp(target_ts, 0).unwrap_or(now)
}

// ── Parsing helpers ───────────────────────────────────────────────────────────

fn extract_token_ids(
    flat: &Option<Vec<String>>,
    nested: &Option<Vec<GammaToken>>,
) -> Vec<String> {
    if let Some(ids) = flat {
        let clean: Vec<String> = ids.iter().filter(|s| !s.is_empty()).cloned().collect();
        if clean.len() >= 2 {
            return clean;
        }
    }
    if let Some(list) = nested {
        return list
            .iter()
            .filter_map(|t| t.token_id.as_deref().filter(|s| !s.is_empty()).map(str::to_string))
            .collect();
    }
    vec![]
}

fn price_from_tokens(tokens: Option<&[GammaToken]>) -> Option<Decimal> {
    tokens?.first().and_then(|t| match &t.price {
        Some(serde_json::Value::Number(n)) => Decimal::from_str(&n.to_string()).ok(),
        Some(serde_json::Value::String(s)) => Decimal::from_str(s).ok(),
        _ => None,
    })
}

pub fn detect_underlying(question: &str) -> Underlying {
    let q = question.to_lowercase();
    if q.contains("bitcoin")
        || q.contains(" btc")
        || q.starts_with("btc")
        || q.contains("btc-usd")
        || q.contains("btcusd")
        || q.contains(" xbt")
    {
        return Underlying::Btc;
    }
    if q.contains("ethereum")
        || q.contains(" eth")
        || q.starts_with("eth")
        || q.contains("eth-usd")
        || q.contains("ethusd")
        || q.contains("ether ")
    {
        return Underlying::Eth;
    }
    Underlying::Other
}

/// Parse a market expiry string in any of several common formats.
pub fn parse_expiry(s: &str) -> Result<DateTime<Utc>> {
    let s = s.trim();

    // RFC 3339 (handles "Z", "+00:00", milliseconds, etc.)
    if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        return Ok(t.with_timezone(&Utc));
    }
    // "YYYY-MM-DDTHH:MM:SS"
    if let Ok(t) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S") {
        return Ok(t.and_utc());
    }
    // "YYYY-MM-DD HH:MM:SS"
    if let Ok(t) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S") {
        return Ok(t.and_utc());
    }
    // "YYYY-MM-DDTHH:MM" / "YYYY-MM-DD HH:MM"
    if let Ok(t) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M") {
        return Ok(t.and_utc());
    }
    if let Ok(t) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M") {
        return Ok(t.and_utc());
    }
    // "YYYY-MM-DD"  → treat as end-of-day UTC
    if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        let t = d.and_hms_opt(23, 59, 59).expect("valid time");
        return Ok(t.and_utc());
    }
    // Unix epoch integer
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

    // ── detect_underlying ─────────────────────────────────────────────────────

    #[test]
    fn test_detect_underlying() {
        assert_eq!(detect_underlying("Will Bitcoin exceed $70k?"), Underlying::Btc);
        assert_eq!(detect_underlying("ETH price above 4000 on Dec 31?"), Underlying::Eth);
        assert_eq!(detect_underlying("Will it rain tomorrow?"), Underlying::Other);

        assert_eq!(detect_underlying("BTC up or down in the next 5 minutes?"), Underlying::Btc);
        assert_eq!(detect_underlying("Will BTC-USD be higher at 10:05?"), Underlying::Btc);
        assert_eq!(detect_underlying("ETH-USD 5 min up/down"), Underlying::Eth);
        assert_eq!(detect_underlying("Ethereum price up in 15 min?"), Underlying::Eth);
    }

    // ── parse_expiry ──────────────────────────────────────────────────────────

    #[test]
    fn test_parse_expiry_rfc3339()       { assert_eq!(parse_expiry("2025-03-30T10:05:00Z").unwrap().format("%H:%M").to_string(), "10:05"); }
    #[test]
    fn test_parse_expiry_rfc3339_millis(){ assert_eq!(parse_expiry("2025-03-30T10:05:00.000Z").unwrap().format("%H:%M").to_string(), "10:05"); }
    #[test]
    fn test_parse_expiry_space_sep()     { assert_eq!(parse_expiry("2025-03-30 10:05:00").unwrap().format("%H:%M").to_string(), "10:05"); }
    #[test]
    fn test_parse_expiry_no_seconds()    { assert_eq!(parse_expiry("2025-03-30T10:05").unwrap().format("%H:%M").to_string(), "10:05"); }
    #[test]
    fn test_parse_expiry_date_only()     { assert_eq!(parse_expiry("2025-03-30").unwrap().format("%H:%M:%S").to_string(), "23:59:59"); }
    #[test]
    fn test_parse_expiry_unix() {
        let secs = DateTime::parse_from_rfc3339("2025-03-30T10:05:00Z").unwrap().timestamp();
        assert_eq!(parse_expiry(&secs.to_string()).unwrap().format("%H:%M").to_string(), "10:05");
    }

    // ── cycle_target_expiry ───────────────────────────────────────────────────

    #[test]
    fn test_cycle_target_plenty_of_time() {
        // now = 10:01:00 UTC, interval = 300 s, min_entry = 60 s
        // current boundary = 10:05:00, remaining = 240 s > 60 → use current boundary
        let now = DateTime::parse_from_rfc3339("2025-03-30T10:01:00Z").unwrap().with_timezone(&Utc);
        let target = cycle_target_expiry(300, 60, now);
        assert_eq!(target.format("%H:%M:%S").to_string(), "10:05:00");
    }

    #[test]
    fn test_cycle_target_too_little_time() {
        // now = 10:04:30 UTC, interval = 300 s, min_entry = 60 s
        // current boundary = 10:05:00, remaining = 30 s < 60 → pre-fetch next = 10:10:00
        let now = DateTime::parse_from_rfc3339("2025-03-30T10:04:30Z").unwrap().with_timezone(&Utc);
        let target = cycle_target_expiry(300, 60, now);
        assert_eq!(target.format("%H:%M:%S").to_string(), "10:10:00");
    }

    #[test]
    fn test_cycle_target_15m() {
        // now = 10:05:00, interval = 900 s, boundary = 10:15:00, remaining = 600 > 60 → 10:15
        let now = DateTime::parse_from_rfc3339("2025-03-30T10:05:00Z").unwrap().with_timezone(&Utc);
        let target = cycle_target_expiry(900, 60, now);
        assert_eq!(target.format("%H:%M:%S").to_string(), "10:15:00");
    }

    // ── extract_token_ids ─────────────────────────────────────────────────────

    #[test]
    fn test_extract_flat_preferred() {
        let flat = Some(vec!["yes".to_string(), "no".to_string()]);
        let nested = Some(vec![
            GammaToken { token_id: Some("other".to_string()), outcome: None, price: None },
            GammaToken { token_id: Some("other2".to_string()), outcome: None, price: None },
        ]);
        assert_eq!(extract_token_ids(&flat, &nested)[0], "yes");
    }

    #[test]
    fn test_extract_nested_fallback() {
        let flat: Option<Vec<String>> = None;
        let nested = Some(vec![
            GammaToken { token_id: Some("ny".to_string()), outcome: None, price: None },
            GammaToken { token_id: Some("nn".to_string()), outcome: None, price: None },
        ]);
        let ids = extract_token_ids(&flat, &nested);
        assert_eq!(ids[0], "ny");
        assert_eq!(ids[1], "nn");
    }

    #[test]
    fn test_extract_empty_flat_falls_back() {
        let flat = Some(vec!["".to_string(), "".to_string()]);
        let nested = Some(vec![
            GammaToken { token_id: Some("fb_yes".to_string()), outcome: None, price: None },
            GammaToken { token_id: Some("fb_no".to_string()), outcome: None, price: None },
        ]);
        assert_eq!(extract_token_ids(&flat, &nested)[0], "fb_yes");
    }

    // ── active/closed serde defaults ──────────────────────────────────────────

    #[test]
    fn test_active_closed_defaults() {
        let json = r#"{"conditionId":"0xabc","question":"BTC up in 5min?"}"#;
        let m: GammaMarket = serde_json::from_str(json).unwrap();
        assert!(m.active,   "absent active  → should default true");
        assert!(!m.closed,  "absent closed  → should default false");
        assert!(!m.archived,"absent archived → should default false");
    }
}
