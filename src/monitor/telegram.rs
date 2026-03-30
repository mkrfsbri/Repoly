use anyhow::Result;
use rust_decimal::Decimal;
use tracing::{debug, warn};

#[derive(Debug, Clone)]
pub enum AlertKind {
    Entry {
        market: String,
        side: String, // "YES" or "NO"
        score: f64,
        size: Decimal,
        kelly_pct: f64,
        rsi: f64,
        macd_dir: String,
        stoch_dir: String,
        atr_regime: String,
    },
    Exit {
        market: String,
        side: String,
        pnl: Decimal,
        hold_bars: u32,
        win_count: u32,
        loss_count: u32,
    },
    CircuitTrip {
        drawdown_pct: f64,
        reason: String,
    },
    DailySummary {
        pnl: Decimal,
        wins: u32,
        losses: u32,
        best_trade: Decimal,
        balance: Decimal,
        deployed: Decimal,
    },
    Skipped {
        market: String,
        reason: String,
        score: f64,
    },
    Custom(String),
}

impl AlertKind {
    pub fn format(&self) -> String {
        match self {
            AlertKind::Entry {
                market, side, score, size, kelly_pct,
                rsi, macd_dir, stoch_dir, atr_regime,
            } => {
                format!(
                    "✅ ENTRY   | {market}-{side} | Score: {score:.1}/7 | Size: ${size} | Kelly: {:.0}%\n           | RSI:{rsi:.0} MACD:{macd_dir} Stoch:{stoch_dir} ATR:{atr_regime}",
                    kelly_pct * 100.0
                )
            }

            AlertKind::Exit { market, side, pnl, hold_bars, win_count, loss_count } => {
                let sign = if *pnl >= Decimal::ZERO { "+" } else { "" };
                format!(
                    "❌ EXIT    | {market}-{side} | PnL: {sign}{pnl} | Hold: {hold_bars} bars | W/L: {win_count}/{loss_count}"
                )
            }

            AlertKind::CircuitTrip { drawdown_pct, reason } => {
                format!(
                    "🔴 CIRCUIT | OPEN | Drawdown: {drawdown_pct:.1}% | {reason} | All positions closed"
                )
            }

            AlertKind::DailySummary { pnl, wins, losses, best_trade, balance, deployed } => {
                let sign = if *pnl >= Decimal::ZERO { "+" } else { "" };
                format!(
                    "📊 DAILY   | PnL: {sign}{pnl} | {wins}W {losses}L | Best: +{best_trade}\n           | Balance: ${balance} | Deployed: ${deployed}"
                )
            }

            AlertKind::Skipped { market, reason, score } => {
                format!("⚠️  SKIPPED | {market} | {reason} | Score: {score:.1} blocked")
            }

            AlertKind::Custom(msg) => msg.clone(),
        }
    }
}

pub struct TelegramAlert {
    bot_token: String,
    chat_id: String,
    http: reqwest::Client,
    enabled: bool,
}

impl TelegramAlert {
    pub fn new(bot_token: Option<String>, chat_id: Option<String>, enabled: bool) -> Self {
        Self {
            bot_token: bot_token.unwrap_or_default(),
            chat_id: chat_id.unwrap_or_default(),
            http: reqwest::Client::new(),
            enabled,
        }
    }

    pub async fn send(&self, kind: AlertKind) -> Result<()> {
        let text = kind.format();

        if !self.enabled || self.bot_token.is_empty() || self.chat_id.is_empty() {
            debug!("Telegram (disabled): {text}");
            return Ok(());
        }

        let url = format!(
            "https://api.telegram.org/bot{}/sendMessage",
            self.bot_token
        );

        let payload = serde_json::json!({
            "chat_id": self.chat_id,
            "text": text,
            "parse_mode": "HTML"
        });

        let resp = self
            .http
            .post(&url)
            .json(&payload)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("Telegram send failed: {e}"))?;

        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            warn!("Telegram API error: {body}");
        }

        Ok(())
    }
}
