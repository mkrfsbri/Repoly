use anyhow::{bail, Result};
use futures::{SinkExt, StreamExt};
use rust_decimal::Decimal;
use serde::Deserialize;
use std::collections::VecDeque;
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};
use tokio::time::{sleep, Duration};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, error, info, warn};

// ── Public types ────────────────────────────────────────────────────────────

/// A single closed (or live) OHLCV bar from Binance.
#[derive(Debug, Clone)]
pub struct KlineBar {
    pub open: Decimal,
    pub high: Decimal,
    pub low: Decimal,
    pub close: Decimal,
    pub volume: Decimal,
    pub timestamp: i64,
    pub is_closed: bool,
}

/// Ring buffer of up to `capacity` KlineBars per pair per interval.
#[derive(Debug)]
pub struct KlineBuffer {
    bars: VecDeque<KlineBar>,
    capacity: usize,
}

impl KlineBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            bars: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    pub fn push(&mut self, bar: KlineBar) {
        if self.bars.len() == self.capacity {
            self.bars.pop_front();
        }
        self.bars.push_back(bar);
    }

    pub fn len(&self) -> usize {
        self.bars.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bars.is_empty()
    }

    /// Latest bar (most recent).
    pub fn last(&self) -> Option<&KlineBar> {
        self.bars.back()
    }

    /// Iterator from oldest to newest.
    pub fn iter(&self) -> impl Iterator<Item = &KlineBar> {
        self.bars.iter()
    }

    /// Collect close prices oldest → newest.
    pub fn closes(&self) -> Vec<f64> {
        self.bars
            .iter()
            .filter_map(|b| b.close.to_string().parse::<f64>().ok())
            .collect()
    }

    /// Collect (high, low, close) tuples.
    pub fn hlc(&self) -> Vec<(f64, f64, f64)> {
        self.bars
            .iter()
            .filter_map(|b| {
                let h = b.high.to_string().parse::<f64>().ok()?;
                let l = b.low.to_string().parse::<f64>().ok()?;
                let c = b.close.to_string().parse::<f64>().ok()?;
                Some((h, l, c))
            })
            .collect()
    }

    /// Collect (close, volume) pairs.
    pub fn close_volume(&self) -> Vec<(f64, f64)> {
        self.bars
            .iter()
            .filter_map(|b| {
                let c = b.close.to_string().parse::<f64>().ok()?;
                let v = b.volume.to_string().parse::<f64>().ok()?;
                Some((c, v))
            })
            .collect()
    }
}

// ── Internal serde structs ───────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct StreamMsg {
    stream: String,
    data: KlineEvent,
}

#[derive(Debug, Deserialize)]
struct KlineEvent {
    #[serde(rename = "k")]
    kline: RawKline,
}

#[derive(Debug, Deserialize)]
struct RawKline {
    #[serde(rename = "t")]
    open_time: i64,
    #[serde(rename = "o", deserialize_with = "de_decimal")]
    open: Decimal,
    #[serde(rename = "h", deserialize_with = "de_decimal")]
    high: Decimal,
    #[serde(rename = "l", deserialize_with = "de_decimal")]
    low: Decimal,
    #[serde(rename = "c", deserialize_with = "de_decimal")]
    close: Decimal,
    #[serde(rename = "v", deserialize_with = "de_decimal")]
    volume: Decimal,
    #[serde(rename = "x")]
    is_closed: bool,
}

fn de_decimal<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Decimal, D::Error> {
    let s = String::deserialize(d)?;
    Decimal::from_str(&s).map_err(serde::de::Error::custom)
}

// ── Historical bootstrap ─────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct HistoricalKline(
    i64,    // open time
    String, // open
    String, // high
    String, // low
    String, // close
    String, // volume
    i64,    // close time
    // remaining fields ignored
    #[serde(default)] serde_json::Value,
    #[serde(default)] serde_json::Value,
    #[serde(default)] serde_json::Value,
    #[serde(default)] serde_json::Value,
    #[serde(default)] serde_json::Value,
);

pub async fn fetch_history(
    rest_url: &str,
    symbol: &str,
    interval: &str,
    limit: usize,
) -> Result<Vec<KlineBar>> {
    let url = format!(
        "{rest_url}/api/v3/klines?symbol={symbol}&interval={interval}&limit={limit}"
    );
    let resp = reqwest::get(&url).await?.json::<Vec<HistoricalKline>>().await?;
    let bars = resp
        .into_iter()
        .map(|k| {
            Ok(KlineBar {
                timestamp: k.0,
                open: Decimal::from_str(&k.1)?,
                high: Decimal::from_str(&k.2)?,
                low: Decimal::from_str(&k.3)?,
                close: Decimal::from_str(&k.4)?,
                volume: Decimal::from_str(&k.5)?,
                is_closed: true,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(bars)
}

// ── BinanceFeed ──────────────────────────────────────────────────────────────

/// Shared, thread-safe map of `"symbol_interval"` → KlineBuffer.
pub type BufferMap = Arc<RwLock<std::collections::HashMap<String, KlineBuffer>>>;

pub struct BinanceFeed {
    ws_url: String,
    streams: Vec<String>,
    /// Broadcast channel for closed bar events: (stream_key, KlineBar)
    pub tx: broadcast::Sender<(String, KlineBar)>,
    pub buffers: BufferMap,
    buffer_capacity: usize,
}

impl BinanceFeed {
    pub fn new(ws_url: String, streams: Vec<String>, buffer_capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(1024);
        let buffers = Arc::new(RwLock::new(std::collections::HashMap::new()));
        Self {
            ws_url,
            streams,
            tx,
            buffers,
            buffer_capacity,
        }
    }

    /// Subscribe to closed-bar events.
    pub fn subscribe(&self) -> broadcast::Receiver<(String, KlineBar)> {
        self.tx.subscribe()
    }

    /// Spawn the WS listener task with automatic reconnect.
    pub async fn run(self: Arc<Self>) {
        let mut backoff = 1u64;
        loop {
            if let Err(e) = self.connect_and_stream().await {
                error!("Binance WS error: {e:#}");
            }
            warn!("Binance WS disconnected, reconnecting in {backoff}s");
            sleep(Duration::from_secs(backoff)).await;
            backoff = (backoff * 2).min(60);
        }
    }

    async fn connect_and_stream(&self) -> Result<()> {
        let stream_param = self.streams.join("/");
        let url = format!("{}/stream?streams={}", self.ws_url, stream_param);
        info!("Connecting to Binance WS: {url}");

        let (ws_stream, _) = connect_async(&url).await?;
        let (_write, mut read) = ws_stream.split();

        // Reset backoff on successful connect
        while let Some(msg) = read.next().await {
            match msg? {
                Message::Text(text) => {
                    if let Err(e) = self.handle_message(&text).await {
                        debug!("WS message parse error: {e}");
                    }
                }
                Message::Ping(_) => {}
                Message::Close(_) => bail!("Server closed WS connection"),
                _ => {}
            }
        }
        Ok(())
    }

    async fn handle_message(&self, text: &str) -> Result<()> {
        let msg: StreamMsg = serde_json::from_str(text)?;
        let raw = &msg.data.kline;

        // Stream name: "btcusdt@kline_5m" → key "BTCUSDT_5m"
        let key = stream_key_from_stream(&msg.stream);

        let bar = KlineBar {
            open: raw.open,
            high: raw.high,
            low: raw.low,
            close: raw.close,
            volume: raw.volume,
            timestamp: raw.open_time,
            is_closed: raw.is_closed,
        };

        {
            let mut map = self.buffers.write().await;
            let buf = map
                .entry(key.clone())
                .or_insert_with(|| KlineBuffer::new(self.buffer_capacity));
            if bar.is_closed {
                buf.push(bar.clone());
            }
        }

        if bar.is_closed {
            debug!(stream = %key, close = %bar.close, "Closed kline");
            let _ = self.tx.send((key, bar));
        }

        Ok(())
    }
}

/// "btcusdt@kline_5m" → "BTCUSDT_5m"
pub fn stream_key_from_stream(stream: &str) -> String {
    // e.g. "btcusdt@kline_5m"
    let parts: Vec<&str> = stream.splitn(2, '@').collect();
    if parts.len() == 2 {
        let symbol = parts[0].to_uppercase();
        let interval = parts[1].replace("kline_", "");
        format!("{symbol}_{interval}")
    } else {
        stream.to_uppercase()
    }
}

// ── Unit tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stream_key_parse() {
        assert_eq!(stream_key_from_stream("btcusdt@kline_5m"), "BTCUSDT_5m");
        assert_eq!(stream_key_from_stream("btcusdt@kline_15m"), "BTCUSDT_15m");
        assert_eq!(stream_key_from_stream("ethusdt@kline_5m"), "ETHUSDT_5m");
        assert_eq!(stream_key_from_stream("ethusdt@kline_15m"), "ETHUSDT_15m");
    }

    #[test]
    fn test_kline_bar_parse() {
        let json = r#"{
            "stream": "btcusdt@kline_5m",
            "data": {
                "e": "kline",
                "E": 1700000000000,
                "s": "BTCUSDT",
                "k": {
                    "t": 1700000000000,
                    "T": 1700000299999,
                    "s": "BTCUSDT",
                    "i": "5m",
                    "f": 100,
                    "L": 200,
                    "o": "67000.00",
                    "h": "67500.00",
                    "l": "66800.00",
                    "c": "67300.00",
                    "v": "123.45",
                    "n": 100,
                    "x": true,
                    "q": "8275650.00",
                    "V": "60.00",
                    "Q": "4032000.00",
                    "B": "0"
                }
            }
        }"#;
        let msg: StreamMsg = serde_json::from_str(json).unwrap();
        assert!(msg.data.kline.is_closed);
        assert_eq!(msg.data.kline.close, Decimal::from_str("67300.00").unwrap());
    }

    #[test]
    fn test_ring_buffer_capacity() {
        let mut buf = KlineBuffer::new(3);
        for i in 0..5u64 {
            buf.push(KlineBar {
                open: Decimal::ZERO,
                high: Decimal::ZERO,
                low: Decimal::ZERO,
                close: Decimal::from(i),
                volume: Decimal::ONE,
                timestamp: i as i64,
                is_closed: true,
            });
        }
        assert_eq!(buf.len(), 3);
        assert_eq!(buf.last().unwrap().close, Decimal::from(4u64));
    }
}
