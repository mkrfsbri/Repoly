mod backtest;
mod config;
mod executor;
mod feeds;
mod indicators;
mod monitor;
mod risk;
mod signals;

use anyhow::{Context, Result};
use chrono::Utc;
use ethers::providers::{Provider, Ws};
use executor::AutoClaimer;
use feeds::binance_ws::{fetch_history, stream_key_from_stream, BinanceFeed, KlineBuffer};
use feeds::gamma_api::GammaClient;
use indicators::IndicatorBundle;
use monitor::tui::SharedDashboard;
use risk::circuit::CircuitBreaker;
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::signal;
use tokio::sync::{broadcast, RwLock};
use tracing::{error, info, warn};
use tracing_subscriber::{fmt, EnvFilter};

#[tokio::main]
async fn main() -> Result<()> {
    // ── Load config ───────────────────────────────────────────────────────────
    let config_path = std::env::var("CONFIG_PATH").unwrap_or("config.toml".to_string());
    let cfg = config::Config::load(&config_path)
        .context("Failed to load config")?;

    // ── Tracing setup ─────────────────────────────────────────────────────────
    let filter = EnvFilter::new(&cfg.bot.log_level);
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .compact()
        .init();

    info!("Polymarket Signal Bot starting up");
    info!("Mode: {}", if cfg.bot.dry_run { "DRY RUN" } else { "LIVE" });

    // ── Auth (optional in dry-run) ────────────────────────────────────────────
    let auth_result = executor::signing::PolyAuth::from_env();
    let auth = match auth_result {
        Ok(a) => {
            info!("Wallet address: {:?}", a.address());
            Some(a)
        }
        Err(e) => {
            if cfg.bot.dry_run {
                warn!("Auth not configured (ok in dry-run): {e}");
                None
            } else {
                return Err(e.context("Auth required for live trading"));
            }
        }
    };

    // ── Gamma API market discovery ────────────────────────────────────────────
    let gamma = Arc::new(GammaClient::new(
        cfg.gamma.base_url.clone(),
        cfg.gamma.min_volume_24h,
        cfg.gamma.max_resolution_minutes,
    ));
    let gamma_clone = gamma.clone();
    let gamma_interval = cfg.gamma.refresh_interval_secs;
    tokio::spawn(async move {
        gamma_clone.run(gamma_interval).await;
    });

    // ── Binance WebSocket feed ─────────────────────────────────────────────────
    let feed = Arc::new(BinanceFeed::new(
        cfg.binance.ws_url.clone(),
        cfg.binance.streams.clone(),
        cfg.binance.kline_buffer_size,
    ));

    // Bootstrap history for each stream
    info!("Bootstrapping historical klines...");
    for stream in &cfg.binance.streams {
        // "btcusdt@kline_5m" → symbol "BTCUSDT", interval "5m"
        if let Some((sym, interval)) = parse_stream(stream) {
            match fetch_history(
                &cfg.binance.rest_url,
                &sym,
                &interval,
                cfg.binance.kline_buffer_size,
            )
            .await
            {
                Ok(bars) => {
                    let key = stream_key_from_stream(stream);
                    let mut map = feed.buffers.write().await;
                    let buf = map
                        .entry(key.clone())
                        .or_insert_with(|| KlineBuffer::new(cfg.binance.kline_buffer_size));
                    for bar in bars {
                        buf.push(bar);
                    }
                    info!("Bootstrapped {key}: {} bars", buf.len());
                }
                Err(e) => warn!("History bootstrap failed for {stream}: {e}"),
            }
        }
    }

    // Start WS listener
    let feed_clone = feed.clone();
    tokio::spawn(async move { feed_clone.run().await });

    // ── TUI dashboard ─────────────────────────────────────────────────────────
    let (tui_app, dashboard_state) = monitor::tui::TuiApp::new();

    // ── Circuit breaker ───────────────────────────────────────────────────────
    let circuit = CircuitBreaker::new(
        Decimal::new(1000, 0), // placeholder initial balance
        cfg.circuit_breaker.max_drawdown,
        cfg.circuit_breaker.max_consecutive_losses,
        cfg.circuit_breaker.max_api_error_rate,
    );

    // ── Telegram alerts ───────────────────────────────────────────────────────
    let telegram = Arc::new(monitor::telegram::TelegramAlert::new(
        cfg.telegram.bot_token.clone(),
        cfg.telegram.chat_id.clone(),
        cfg.telegram.enabled,
    ));

    // ── Auto-claim loop ───────────────────────────────────────────────────────
    if cfg.claim.enabled {
        match setup_auto_claimer(&cfg, auth.as_ref()).await {
            Ok(claimer) => {
                let claimer_task = claimer.clone();
                let tg_claim = telegram.clone();
                tokio::spawn(async move {
                    loop {
                        tokio::time::sleep(tokio::time::Duration::from_secs(
                            claimer_task.check_interval_secs,
                        ))
                        .await;
                        match claimer_task.claim_cycle().await {
                            Ok(results) => {
                                for r in results {
                                    info!(
                                        condition_id = %r.condition_id,
                                        amount_usdc = %r.amount_usdc,
                                        tx = %r.tx_hash,
                                        "Position claimed"
                                    );
                                    let _ = tg_claim
                                        .send(monitor::telegram::AlertKind::Claim {
                                            condition_id: r.condition_id,
                                            amount_usdc: r.amount_usdc,
                                            tx_hash: r.tx_hash,
                                            via_relayer: r.via_relayer,
                                        })
                                        .await;
                                }
                            }
                            Err(e) => warn!("Claim cycle error: {e}"),
                        }
                    }
                });
                info!("AutoClaimer task started");
            }
            Err(e) => {
                warn!("AutoClaimer setup failed (claim loop disabled): {e}");
            }
        }
    }

    // ── Main signal loop ──────────────────────────────────────────────────────
    let mut rx = feed.subscribe();
    let cfg_signal = cfg.signal.clone();
    let cfg_vol = cfg.volatility.clone();
    let dashboard_clone = dashboard_state.clone();
    let circuit_clone = circuit.clone();
    let telegram_clone = telegram.clone();

    // Per-stream indicator bundles
    let indicators: Arc<RwLock<HashMap<String, IndicatorBundle>>> =
        Arc::new(RwLock::new(HashMap::new()));

    // Per-stream signal machines
    let machines: Arc<RwLock<HashMap<String, signals::SignalMachine>>> =
        Arc::new(RwLock::new(HashMap::new()));

    tokio::spawn(async move {
        info!("Signal loop started");
        loop {
            match rx.recv().await {
                Ok((key, bar)) => {
                    if !circuit_clone.is_ok() {
                        continue;
                    }

                    // Convert Decimal → f64. Skip bar entirely if any price is
                    // non-finite or zero — feeding 0.0 to indicators corrupts their state.
                    let (close, high, low, vol) = {
                        let parse = |d: &rust_decimal::Decimal| -> Option<f64> {
                            let v: f64 = d.to_string().parse().ok()?;
                            if v.is_finite() && v > 0.0 { Some(v) } else { None }
                        };
                        match (parse(&bar.close), parse(&bar.high), parse(&bar.low), parse(&bar.volume)) {
                            (Some(c), Some(h), Some(l), Some(v)) => (c, h, l, v),
                            _ => {
                                warn!(key = %key, close = %bar.close, "Invalid bar prices, skipping");
                                continue;
                            }
                        }
                    };

                    // Update indicators
                    let (score, macd_sig) = {
                        let mut bundles = indicators.write().await;
                        let bundle = bundles
                            .entry(key.clone())
                            .or_insert_with(IndicatorBundle::new);

                        bundle.rsi.update(close);
                        bundle.ema.update(close);
                        let macd_sig = bundle.macd.update(close).unwrap_or(indicators::MacdSignal::Neutral);
                        bundle.stoch.update(high, low, close);
                        bundle.obv.update(close, vol);
                        bundle.vwap.update(
                            high,
                            low,
                            close,
                            vol,
                            chrono::DateTime::from_timestamp_millis(bar.timestamp)
                                .unwrap_or(Utc::now()),
                        );
                        bundle.atr.update(high, low, close);

                        if !bundle.is_ready() {
                            continue;
                        }

                        let s = signals::scorer::ConfluenceScore::compute(
                            close,
                            &bundle.rsi,
                            &macd_sig,
                            &bundle.stoch,
                            &bundle.ema,
                            &bundle.obv,
                            &bundle.vwap,
                            &bundle.atr,
                            cfg_vol.atr_min_pct,
                            cfg_vol.atr_max_pct,
                        );
                        (s, macd_sig)
                    };

                    // Tick the state machine
                    let triggered = {
                        let mut mach_map = machines.write().await;
                        let machine = mach_map
                            .entry(key.clone())
                            .or_insert_with(|| signals::SignalMachine::new(
                                &key,
                                cfg_signal.cooldown_bars,
                                cfg_signal.reentry_cooldown_bars,
                            ));

                        let state = machine.tick(score.clone(), cfg_signal.confluence_threshold);
                        *state == signals::SignalState::Triggered
                    };

                    if triggered {
                        let dir = score.direction();
                        info!(
                            key = %key,
                            score = score.total,
                            direction = ?dir,
                            "🚀 SIGNAL TRIGGERED"
                        );

                        // Log to dashboard
                        let mut dash = dashboard_clone.write().await;
                        dash.push_log(format!(
                            "{} {key} TRIGGER score={:.1}",
                            Utc::now().format("%H:%M"),
                            score.total
                        ));
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!("Signal loop lagged, dropped {n} events");
                }
                Err(broadcast::error::RecvError::Closed) => {
                    error!("Broadcast channel closed");
                    break;
                }
            }
        }
    });

    info!("Press 'q' in TUI or Ctrl+C to exit");

    // Run TUI in a blocking thread so it doesn't block the async runtime
    let tui_handle = tokio::task::spawn_blocking(move || {
        if let Err(e) = tui_app.run() {
            error!("TUI error: {e}");
        }
    });

    // Wait for TUI exit or Ctrl+C
    tokio::select! {
        _ = tui_handle => {
            info!("TUI exited");
        }
        _ = signal::ctrl_c() => {
            info!("Received Ctrl+C, shutting down");
        }
    }

    info!("Shutdown complete");
    Ok(())
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Build an AutoClaimer from config + optional auth. Returns Err if auth is
/// missing (required for signing redemption headers) or RPC connection fails.
async fn setup_auto_claimer(
    cfg: &config::Config,
    auth: Option<&executor::signing::PolyAuth>,
) -> Result<Arc<AutoClaimer>> {
    let auth = auth
        .cloned()
        .context("AutoClaimer requires wallet credentials (POLY_API_KEY etc.)")?;

    let wallet = auth
        .local_wallet()
        .context("AutoClaimer requires a local wallet for on-chain fallback")?;

    let provider: Provider<Ws> = Provider::<Ws>::connect(&cfg.polygon.rpc_url)
        .await
        .context("AutoClaimer: failed to connect to Polygon RPC")?;
    let provider = Arc::new(provider);

    let claimer = Arc::new(
        AutoClaimer::new(
            cfg.clob.base_url.clone(),
            &cfg.claim.ctf_address,
            &cfg.polygon.usdc_address,
            &cfg.claim.neg_risk_adapter,
            provider,
            wallet,
            auth,
            cfg.bot.dry_run,
            cfg.claim.use_relayer,
            cfg.claim.check_interval_secs,
            cfg.claim.min_claimable_usdc,
        )
        .context("Failed to construct AutoClaimer")?,
    );

    Ok(claimer)
}

/// "btcusdt@kline_5m" → Some(("BTCUSDT", "5m"))
fn parse_stream(stream: &str) -> Option<(String, String)> {
    let parts: Vec<&str> = stream.splitn(2, '@').collect();
    if parts.len() == 2 {
        let sym = parts[0].to_uppercase();
        let interval = parts[1].replace("kline_", "");
        Some((sym, interval))
    } else {
        None
    }
}
