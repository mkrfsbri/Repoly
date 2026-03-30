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
use executor::clob::{ClobClient, OrderRequest, OrderSide};
use executor::positions::{OpenPosition, PositionTracker};
use executor::signing::PolyAuth;
use executor::AutoClaimer;
use feeds::binance_ws::{fetch_history, stream_key_from_stream, BinanceFeed, KlineBuffer};
use feeds::gamma_api::{GammaClient, Underlying};
use indicators::IndicatorBundle;
use monitor::tui::{ClaimDisplay, PositionDisplay, SharedDashboard};
use risk::circuit::CircuitBreaker;
use risk::kelly::KellySizer;
use rust_decimal::Decimal;
use signals::conflict::{resolve_conflicts, Signal as ConflictSignal};
use signals::scorer::Direction;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use tokio::signal;
use tokio::sync::{broadcast, Mutex, RwLock};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    // ── Load config ───────────────────────────────────────────────────────────
    let config_path = std::env::var("CONFIG_PATH").unwrap_or_else(|_| "config.toml".to_string());
    let cfg = config::Config::load(&config_path).context("Failed to load config")?;

    // ── Tracing setup ─────────────────────────────────────────────────────────
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new(&cfg.bot.log_level))
        .with_target(false)
        .compact()
        .init();

    info!("Polymarket Signal Bot starting up");
    info!("Mode: {}", if cfg.bot.dry_run { "DRY RUN" } else { "LIVE" });

    // ── Auth (optional in dry-run) ────────────────────────────────────────────
    let auth: Option<PolyAuth> = match PolyAuth::from_env() {
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
        cfg.gamma.interval_secs.clone(),
        cfg.gamma.min_entry_secs,
    ));
    {
        let g = gamma.clone();
        let secs = cfg.gamma.refresh_interval_secs;
        tokio::spawn(async move { g.run(secs).await });
    }

    // ── Binance WebSocket feed ────────────────────────────────────────────────
    let feed = Arc::new(BinanceFeed::new(
        cfg.binance.ws_url.clone(),
        cfg.binance.streams.clone(),
        cfg.binance.kline_buffer_size,
    ));

    info!("Bootstrapping historical klines...");
    for stream in &cfg.binance.streams {
        if let Some((sym, interval)) = parse_stream(stream) {
            match fetch_history(&cfg.binance.rest_url, &sym, &interval, cfg.binance.kline_buffer_size).await {
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
    {
        let f = feed.clone();
        tokio::spawn(async move { f.run().await });
    }

    // ── TUI dashboard ─────────────────────────────────────────────────────────
    let (tui_app, dashboard_state) = monitor::tui::TuiApp::new();

    // ── Risk components ───────────────────────────────────────────────────────
    let initial_balance = cfg.risk.initial_balance()?;

    let circuit = CircuitBreaker::new(
        initial_balance,
        cfg.circuit_breaker.max_drawdown,
        cfg.circuit_breaker.max_consecutive_losses,
        cfg.circuit_breaker.max_api_error_rate,
    );

    let kelly = Arc::new(Mutex::new(KellySizer::new(
        cfg.risk.kelly_window,
        cfg.risk.min_bet()?,
        cfg.risk.max_bet()?,
        cfg.risk.kelly_fraction,
        cfg.risk.cold_start_pct,
    )));

    // In-memory balance tracking. Updated on each order entry/exit.
    let balance_arc: Arc<RwLock<Decimal>> = Arc::new(RwLock::new(initial_balance));

    // ── CLOB client ───────────────────────────────────────────────────────────
    // Use real auth when available; fall back to dummy (all operations are no-ops
    // in dry_run mode so the dummy secret is never actually sent).
    let clob = Arc::new(ClobClient::new(
        cfg.clob.base_url.clone(),
        auth.clone().unwrap_or_else(PolyAuth::dummy),
        cfg.clob.max_retries,
        cfg.clob.order_timeout_secs,
        cfg.bot.dry_run,
    ));

    // ── Position tracker ──────────────────────────────────────────────────────
    let positions: Arc<RwLock<PositionTracker>> = Arc::new(RwLock::new(PositionTracker::new()));

    // ── Tick size (pre-computed once) ─────────────────────────────────────────
    let tick_size = cfg.clob.tick_size_decimal()?;

    // ── Telegram alerts ───────────────────────────────────────────────────────
    let telegram = Arc::new(monitor::telegram::TelegramAlert::new(
        cfg.telegram.bot_token.clone(),
        cfg.telegram.chat_id.clone(),
        cfg.telegram.enabled,
    ));

    // ── Per-stream state (hoisted so exit-monitor + signal-loop can both use) ─
    let indicators: Arc<RwLock<HashMap<String, IndicatorBundle>>> =
        Arc::new(RwLock::new(HashMap::new()));
    let machines: Arc<RwLock<HashMap<String, signals::SignalMachine>>> =
        Arc::new(RwLock::new(HashMap::new()));

    // ── Auto-claim loop ───────────────────────────────────────────────────────
    if cfg.claim.enabled {
        match setup_auto_claimer(&cfg, auth.as_ref()).await {
            Ok(claimer) => {
                let claimer_task = claimer.clone();
                let tg_claim = telegram.clone();
                let dash_claim = dashboard_state.clone();
                tokio::spawn(async move {
                    loop {
                        tokio::time::sleep(tokio::time::Duration::from_secs(
                            claimer_task.check_interval_secs,
                        ))
                        .await;
                        match claimer_task.claim_cycle().await {
                            Ok(results) => {
                                for r in results {
                                    info!(condition_id = %r.condition_id, amount_usdc = %r.amount_usdc, tx = %r.tx_hash, "Position claimed");
                                    let _ = tg_claim
                                        .send(monitor::telegram::AlertKind::Claim {
                                            condition_id: r.condition_id.clone(),
                                            amount_usdc: r.amount_usdc,
                                            tx_hash: r.tx_hash.clone(),
                                            via_relayer: r.via_relayer,
                                        })
                                        .await;
                                    let mut dash = dash_claim.write().await;
                                    dash.push_claim(ClaimDisplay {
                                        condition_id: r.condition_id,
                                        amount_usdc: r.amount_usdc,
                                        tx_hash: r.tx_hash,
                                        via_relayer: r.via_relayer,
                                    });
                                }
                            }
                            Err(e) => warn!("Claim cycle error: {e}"),
                        }
                    }
                });
                info!("AutoClaimer task started");
            }
            Err(e) => warn!("AutoClaimer setup failed (claim loop disabled): {e}"),
        }
    }

    // ── Exit monitor task ─────────────────────────────────────────────────────
    // Polls every 30s for expired positions and settles them.
    {
        let positions = positions.clone();
        let machines = machines.clone();
        let balance = balance_arc.clone();
        let kelly = kelly.clone();
        let circuit = circuit.clone();
        let gamma = gamma.clone();
        let telegram = telegram.clone();
        let dashboard = dashboard_state.clone();

        tokio::spawn(async move {
            loop {
                tokio::time::sleep(tokio::time::Duration::from_secs(30)).await;

                let expired_ids = positions.read().await.expired_condition_ids();
                if expired_ids.is_empty() {
                    continue;
                }

                // Snapshot current market prices from Gamma
                let gamma_markets = gamma.active_markets().await;

                for cid in expired_ids {
                    let pos = match positions.read().await.get_cloned(&cid) {
                        Some(p) => p,
                        None => continue,
                    };

                    // Look up final price from Gamma (post-resolution).
                    // If the market is no longer in the active list, check all cached markets.
                    let final_price = gamma_markets
                        .iter()
                        .find(|m| m.condition_id == cid)
                        .map(|m| m.current_price)
                        .unwrap_or(Decimal::new(50, 2)); // unresolved default

                    // Only settle when the binary outcome is clear (>97% or <3%).
                    let resolved = final_price > Decimal::new(97, 2) || final_price < Decimal::new(3, 2);
                    if !resolved {
                        continue; // await next poll cycle
                    }

                    // Determine win/loss based on which token we hold.
                    let won = match pos.direction {
                        Direction::Long => final_price > Decimal::new(97, 2),  // YES resolved
                        Direction::Short => final_price < Decimal::new(3, 2),  // NO resolved
                    };

                    // PnL: on win receive 1.0 per token; paid entry_price per token.
                    // Tokens held = size_usdc / entry_price.
                    // Payout = tokens * 1.0 = size_usdc / entry_price.
                    // PnL = payout - size_usdc.
                    let pnl = if won {
                        if pos.entry_price.is_zero() {
                            Decimal::ZERO
                        } else {
                            pos.size_usdc / pos.entry_price - pos.size_usdc
                        }
                    } else {
                        -pos.size_usdc
                    };

                    // Update tracked balance (stake was already removed on entry).
                    {
                        let mut bal = balance.write().await;
                        if won {
                            *bal += pos.size_usdc + pnl;
                        }
                        // On loss, stake already deducted — nothing to add back.
                        circuit.check_balance(*bal);
                    }

                    // Inform risk components.
                    kelly.lock().await.record_trade(won, pnl.to_string().parse::<f64>().unwrap_or(0.0));
                    circuit.record_trade(won);

                    // Remove from tracker and accumulate stats.
                    {
                        let mut pt = positions.write().await;
                        pt.record_exit(won, pnl);
                        pt.remove(&cid);
                    }

                    // Advance state machine for the originating stream.
                    {
                        let mut mach_map = machines.write().await;
                        if let Some(machine) = mach_map.get_mut(&pos.stream_key) {
                            machine.mark_exited();
                        }
                    }

                    // Telegram exit alert.
                    let side_str = if matches!(pos.direction, Direction::Long) { "YES" } else { "NO" };
                    let _ = telegram
                        .send(monitor::telegram::AlertKind::Exit {
                            market: cid.clone(),
                            side: side_str.to_string(),
                            pnl,
                            hold_bars: ((Utc::now() - pos.entered_at).num_minutes().unsigned_abs()) as u32,
                            win_count: positions.read().await.wins,
                            loss_count: positions.read().await.losses,
                        })
                        .await;

                    // Dashboard update.
                    {
                        let bal_snap = *balance.read().await;
                        let deployed_snap = positions.read().await.total_deployed();
                        let mut dash = dashboard.write().await;
                        dash.balance = bal_snap;
                        dash.deployed = deployed_snap;
                        dash.today_pnl += pnl;
                        if won { dash.today_wins += 1; } else { dash.today_losses += 1; }
                        // Remove position from dashboard list.
                        dash.positions.retain(|p| p.market != cid);
                        dash.push_log(format!(
                            "{} {} {}-{} pnl={:+.2}",
                            Utc::now().format("%H:%M"),
                            if won { "WIN " } else { "LOSS" },
                            &cid[..8.min(cid.len())],
                            side_str,
                            pnl,
                        ));
                    }
                }
            }
        });
    }

    // ── Main signal loop ──────────────────────────────────────────────────────
    let mut rx = feed.subscribe();
    let cfg_signal = cfg.signal.clone();
    let cfg_risk = cfg.risk.clone();
    let cfg_vol = cfg.volatility.clone();

    let circuit_loop = circuit.clone();
    let kelly_loop = kelly.clone();
    let clob_loop = clob.clone();
    let positions_loop = positions.clone();
    let balance_loop = balance_arc.clone();
    let machines_loop = machines.clone();
    let indicators_loop = indicators.clone();
    let gamma_loop = gamma.clone();
    let dashboard_loop = dashboard_state.clone();
    let telegram_loop = telegram.clone();

    tokio::spawn(async move {
        info!("Signal loop started");
        loop {
            match rx.recv().await {
                Ok((key, bar)) => {
                    if !circuit_loop.is_ok() {
                        continue;
                    }

                    // Convert Decimal → f64, skipping invalid bars.
                    let (close, high, low, vol) = {
                        let parse = |d: &Decimal| -> Option<f64> {
                            let v: f64 = d.to_string().parse().ok()?;
                            if v.is_finite() && v > 0.0 { Some(v) } else { None }
                        };
                        match (parse(&bar.close), parse(&bar.high), parse(&bar.low), parse(&bar.volume)) {
                            (Some(c), Some(h), Some(l), Some(v)) => (c, h, l, v),
                            _ => {
                                warn!(key = %key, "Invalid bar prices, skipping");
                                continue;
                            }
                        }
                    };

                    let bar_ts = chrono::DateTime::from_timestamp_millis(bar.timestamp)
                        .unwrap_or_else(Utc::now);

                    // Update indicators — also capture values needed for alerts/dashboard.
                    let (score, macd_sig, rsi_val, stoch_dir_str) = {
                        let mut bundles = indicators_loop.write().await;
                        let bundle = bundles
                            .entry(key.clone())
                            .or_insert_with(IndicatorBundle::new);

                        bundle.rsi.update(close);
                        bundle.ema.update(close);
                        let macd_sig = bundle.macd.update(close)
                            .unwrap_or(indicators::MacdSignal::Neutral);
                        bundle.stoch.update(high, low, close);
                        bundle.obv.update(close, vol);
                        bundle.vwap.update(high, low, close, vol, bar_ts);
                        bundle.atr.update(high, low, close);

                        if !bundle.is_ready() {
                            continue;
                        }

                        let rsi_val = bundle.rsi.get().unwrap_or(50.0);
                        let stoch_dir_str = if bundle.stoch.is_long_trigger() {
                            "OS↑"
                        } else if bundle.stoch.is_short_trigger() {
                            "OB↓"
                        } else {
                            "mid"
                        };

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
                        (s, macd_sig, rsi_val, stoch_dir_str.to_string())
                    };

                    // Tick the state machine.
                    let triggered = {
                        let mut mach_map = machines_loop.write().await;
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

                    // Update dashboard market state.
                    {
                        let atr_label = match &score.atr_regime {
                            Some(r) => format!("{r:?}"),
                            None => "?".to_string(),
                        };
                        let sig_state = machines_loop.read().await
                            .get(&key)
                            .map(|m| m.state.label().to_string())
                            .unwrap_or_default();

                        let mut dash = dashboard_loop.write().await;
                        let entry = dash.markets.iter_mut().find(|m| m.symbol == key);
                        let ds = monitor::tui::MarketDisplayState {
                            symbol: key.clone(),
                            price: close,
                            rsi: rsi_val,
                            macd_hist: score.macd_score,
                            stoch_k: 0.0, // stoch k not exposed yet
                            stoch_d: 0.0,
                            ema_dir: if score.ema_score > 0.0 { "↑" } else if score.ema_score < 0.0 { "↓" } else { "→" }.to_string(),
                            obv_status: if score.obv_score > 0.0 { "bull" } else { "bear" }.to_string(),
                            vwap_dev_pct: score.vwap_score * 100.0,
                            atr_regime: atr_label,
                            signal_state: sig_state,
                            signal_score: score.total,
                        };
                        match entry {
                            Some(e) => *e = ds,
                            None => dash.markets.push(ds),
                        }
                        dash.balance = *balance_loop.read().await;
                        dash.deployed = positions_loop.read().await.total_deployed();
                    }

                    if !triggered {
                        continue;
                    }

                    // ── Execute signal ────────────────────────────────────────
                    let dir = match score.direction() {
                        Some(d) => d,
                        None => {
                            warn!(key = %key, "Triggered but no direction, skipping");
                            continue;
                        }
                    };

                    info!(
                        key = %key,
                        score = score.total,
                        direction = ?dir,
                        "🚀 SIGNAL TRIGGERED"
                    );

                    // Find matching Gamma markets for this underlying.
                    let underlying = stream_to_underlying(&key);
                    let matching: Vec<_> = gamma_loop
                        .active_markets()
                        .await
                        .into_iter()
                        .filter(|m| underlying.as_ref() == Some(&m.underlying))
                        .collect();

                    if matching.is_empty() {
                        warn!(key = %key, "No active Gamma markets for signal, skipping entry");
                        dashboard_loop.write().await.push_log(format!(
                            "{} {} TRIGGER score={:.1} — no markets",
                            Utc::now().format("%H:%M"), key, score.total
                        ));
                        continue;
                    }

                    // Build Signal structs and resolve conflicts / capital allocation.
                    let pending: Vec<ConflictSignal> = {
                        let pt = positions_loop.read().await;
                        matching
                            .iter()
                            .filter(|m| !pt.contains(&m.condition_id))
                            .map(|m| ConflictSignal {
                                market_id: m.condition_id.clone(),
                                direction: dir.clone(),
                                score: score.clone(),
                                suggested_size_pct: 0.0,
                            })
                            .collect()
                    };

                    if pending.is_empty() {
                        info!(key = %key, "Already in all matching markets, skipping");
                        continue;
                    }

                    let resolved = resolve_conflicts(
                        pending,
                        cfg_signal.max_concurrent_positions,
                        cfg_risk.max_position_pct,
                        cfg_risk.max_deployed_pct,
                    );

                    let cur_balance = *balance_loop.read().await;

                    for sig in &resolved {
                        // Capital guard.
                        let deployed = positions_loop.read().await.total_deployed();
                        let max_deployed = cur_balance
                            * Decimal::from_str(&format!("{:.6}", cfg_risk.max_deployed_pct))
                                .unwrap_or(Decimal::new(8, 1));
                        if deployed >= max_deployed {
                            warn!("Capital limit reached, no more entries this cycle");
                            break;
                        }

                        let market = match matching.iter().find(|m| m.condition_id == sig.market_id) {
                            Some(m) => m,
                            None => continue,
                        };

                        // Size via Kelly.
                        let size = kelly_loop.lock().await.calculate_size(cur_balance);

                        // Token and price depend on direction.
                        let (token_id, raw_price) = match &sig.direction {
                            Direction::Long => (market.token_yes_id.clone(), market.current_price),
                            Direction::Short => (market.token_no_id.clone(), Decimal::ONE - market.current_price),
                        };

                        // Round to tick size.
                        let price = round_to_tick(raw_price, tick_size);

                        // Sanity check price range (Polymarket prices must be 0.01–0.99).
                        if price < Decimal::new(1, 2) || price > Decimal::new(99, 2) {
                            warn!(key = %key, price = %price, "Price out of valid range, skipping");
                            continue;
                        }

                        let req = OrderRequest::new(
                            sig.market_id.clone(),
                            token_id.clone(),
                            OrderSide::Buy,
                            price,
                            size,
                        );

                        match clob_loop.submit_with_timeout(&req).await {
                            Ok(order_id) => {
                                circuit_loop.record_api_call(true);
                                info!(
                                    market = %sig.market_id,
                                    direction = ?sig.direction,
                                    price = %price,
                                    size = %size,
                                    order_id = %order_id,
                                    "Order placed"
                                );

                                // Transition state machine.
                                machines_loop
                                    .write()
                                    .await
                                    .get_mut(&key)
                                    .map(|m| m.mark_entered());

                                // Register position.
                                positions_loop.write().await.add(OpenPosition {
                                    condition_id: sig.market_id.clone(),
                                    token_id,
                                    direction: sig.direction.clone(),
                                    entry_price: price,
                                    size_usdc: size,
                                    order_id,
                                    stream_key: key.clone(),
                                    entered_at: Utc::now(),
                                    market_expiry: market.expiry,
                                    question: market.question.clone(),
                                });

                                // Deduct from tracked balance.
                                *balance_loop.write().await -= size;

                                // Kelly fraction for alert.
                                let kf = kelly_loop.lock().await.current_fraction();

                                // Telegram entry alert.
                                let side_str = if matches!(sig.direction, Direction::Long) { "YES" } else { "NO" };
                                let _ = telegram_loop
                                    .send(monitor::telegram::AlertKind::Entry {
                                        market: sig.market_id.clone(),
                                        side: side_str.to_string(),
                                        score: score.total,
                                        size,
                                        kelly_pct: kf,
                                        rsi: rsi_val,
                                        macd_dir: format!("{:?}", macd_sig),
                                        stoch_dir: stoch_dir_str.clone(),
                                        atr_regime: score.atr_regime.as_ref().map(|r| format!("{r:?}")).unwrap_or_default(),
                                    })
                                    .await;

                                // Dashboard.
                                {
                                    let bal_snap = *balance_loop.read().await;
                                    let deployed_snap = positions_loop.read().await.total_deployed();
                                    let mut dash = dashboard_loop.write().await;
                                    dash.balance = bal_snap;
                                    dash.deployed = deployed_snap;
                                    dash.positions.push(PositionDisplay {
                                        market: sig.market_id.clone(),
                                        side: side_str.to_string(),
                                        size,
                                        entry_price: price,
                                        current_price: price,
                                        unrealized_pnl: Decimal::ZERO,
                                    });
                                    dash.push_log(format!(
                                        "{} ENTER {}-{} @{:.2} sz={:.0}",
                                        Utc::now().format("%H:%M"),
                                        &sig.market_id[..8.min(sig.market_id.len())],
                                        side_str,
                                        price,
                                        size,
                                    ));
                                }
                            }
                            Err(e) => {
                                warn!(market = %sig.market_id, "Order submission failed: {e}");
                                circuit_loop.record_api_call(false);
                            }
                        }
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

    let tui_handle = tokio::task::spawn_blocking(move || {
        if let Err(e) = tui_app.run() {
            error!("TUI error: {e}");
        }
    });

    tokio::select! {
        _ = tui_handle => info!("TUI exited"),
        _ = signal::ctrl_c() => info!("Received Ctrl+C, shutting down"),
    }

    info!("Shutdown complete");
    Ok(())
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Build an AutoClaimer from config + optional auth.
async fn setup_auto_claimer(
    cfg: &config::Config,
    auth: Option<&PolyAuth>,
) -> Result<Arc<AutoClaimer>> {
    let auth = auth
        .cloned()
        .context("AutoClaimer requires wallet credentials")?;
    let wallet = auth
        .local_wallet()
        .context("AutoClaimer requires a local wallet")?;
    let provider: Provider<Ws> = Provider::<Ws>::connect(&cfg.polygon.rpc_url)
        .await
        .context("AutoClaimer: failed to connect to Polygon RPC")?;

    Ok(Arc::new(
        AutoClaimer::new(
            cfg.clob.base_url.clone(),
            &cfg.claim.ctf_address,
            &cfg.polygon.usdc_address,
            &cfg.claim.neg_risk_adapter,
            Arc::new(provider),
            wallet,
            auth,
            cfg.bot.dry_run,
            cfg.claim.use_relayer,
            cfg.claim.check_interval_secs,
            cfg.claim.min_claimable_usdc,
        )
        .context("Failed to construct AutoClaimer")?,
    ))
}

/// Round a Decimal price to the nearest multiple of `tick`.
fn round_to_tick(price: Decimal, tick: Decimal) -> Decimal {
    if tick.is_zero() {
        return price;
    }
    (price / tick).round() * tick
}

/// Map a Binance stream key (e.g. "btcusdt_5m") to the Polymarket `Underlying`.
fn stream_to_underlying(key: &str) -> Option<Underlying> {
    let k = key.to_lowercase();
    if k.contains("btc") {
        Some(Underlying::Btc)
    } else if k.contains("eth") {
        Some(Underlying::Eth)
    } else {
        None
    }
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
