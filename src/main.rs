//! Application entry point.
//!
//! Boots:
//!   1. Tracing → stderr (filtered by RUST_LOG).
//!   2. Trading client (derives L2 API creds on first use).
//!   3. Chainlink RTDS task (BTC price).
//!   4. Gamma market discovery + auto-roll task.
//!   5. CLOB book subscription task — restarted on each market roll.
//!   6. Crossterm event task.
//!   7. 1-Hz ticker for countdown.
//!
//! All four push into a single `mpsc<AppEvent>`; the main loop drains that,
//! mutates `AppState`, and re-renders.

mod app;
mod config;
mod events;
mod feeds;
mod gamma;
mod trading;
mod ui;

use anyhow::{Context, Result};
use app::{AppEvent, AppState, Outcome, resolve_market_order};
use config::Config;
use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture, Event as CtEvent, EventStream},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use events::Action;
use futures_util::StreamExt;
use ratatui::{backend::CrosstermBackend, Terminal};
use std::{io::stdout, sync::Arc, time::Duration};
use tokio::sync::{mpsc, Mutex};
use tracing::{error, info, warn};
use trading::{OrderArgs, OrderType, Side, TradingClient};

#[tokio::main]
async fn main() -> Result<()> {
    // ── setup ────────────────────────────────────────────────────────
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "btc5m_bot=info,warn".into())
        )
        .with_writer(std::io::stderr)
        .init();

    let cfg = Config::from_env().context("loading config")?;
    info!(signer = %cfg.signer_address, funder = %cfg.funder, "config loaded");

    // Shared event channel — generous buffer so bursts from the book WS don't drop
    let (tx, mut rx) = mpsc::channel::<AppEvent>(512);

    // ── spawn feeds ──────────────────────────────────────────────────
    spawn_price_feed(tx.clone());
    spawn_ticker(tx.clone());
    spawn_key_reader(tx.clone());

    // Trading client lives behind a Mutex so the order-placement task can
    // mutate it without passing ownership around.
    let trading = Arc::new(Mutex::new(TradingClient::new(cfg.clone())?));

    // Best-effort: derive creds on startup (fails silently if offline —
    // user will see the error on first order attempt instead).
    {
        let t = trading.clone();
        tokio::spawn(async move {
            if let Err(e) = t.lock().await.ensure_creds().await {
                warn!(error = %e, "could not derive CLOB API credentials yet");
            }
        });
    }

    // Market discovery + book-subscription supervisor
    let (market_tx, mut market_rx) = mpsc::channel::<gamma::ActiveMarket>(8);
    spawn_market_watcher(tx.clone(), market_tx);

    // When a new market arrives, tear down the old book WS and start a new one.
    let tx_for_books = tx.clone();
    tokio::spawn(async move {
        let mut current: Option<tokio::task::JoinHandle<()>> = None;
        while let Some(m) = market_rx.recv().await {
            if let Some(h) = current.take() { h.abort(); }
            let token_ids = vec![m.up_token_id.clone(), m.down_token_id.clone()];
            current = Some(feeds::clob_ws::spawn(token_ids, clob_forwarder(tx_for_books.clone())));
            let _ = tx_for_books.send(AppEvent::MarketRoll(m)).await;
        }
    });

    // ── terminal setup ───────────────────────────────────────────────
    enable_raw_mode()?;
    let mut out = stdout();
    execute!(out, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(out);
    let mut term = Terminal::new(backend)?;

    let mut state = AppState::new(cfg.default_size_usdc);

    // ── main loop ────────────────────────────────────────────────────
    let result: Result<()> = loop {
        // Render first so startup shows something.
        term.draw(|f| ui::draw(f, &state))?;

        let Some(ev) = rx.recv().await else { break Ok(()) };

        // Key events go through `handle_key`; everything else just applies.
        if let AppEvent::Key(k) = ev {
            let action = events::handle_key(&mut state, k);
            dispatch_action(action, &state, &trading, &tx);
            continue;
        }
        if matches!(ev, AppEvent::Quit) { break Ok(()); }

        state.apply(ev);
    };

    // ── teardown ─────────────────────────────────────────────────────
    disable_raw_mode()?;
    execute!(term.backend_mut(), LeaveAlternateScreen, DisableMouseCapture)?;
    term.show_cursor()?;

    if let Err(e) = &result { error!(error = %e, "exited with error"); }
    result
}

// ── event forwarders ────────────────────────────────────────────────

fn spawn_price_feed(tx: mpsc::Sender<AppEvent>) {
    let (ptx, mut prx) = mpsc::channel::<feeds::chainlink::PriceTick>(64);
    feeds::chainlink::spawn(ptx);
    tokio::spawn(async move {
        while let Some(p) = prx.recv().await {
            let _ = tx.send(AppEvent::Price(p)).await;
        }
    });
}

fn clob_forwarder(tx: mpsc::Sender<AppEvent>) -> mpsc::Sender<feeds::clob_ws::BookSnapshot> {
    let (btx, mut brx) = mpsc::channel::<feeds::clob_ws::BookSnapshot>(64);
    tokio::spawn(async move {
        while let Some(b) = brx.recv().await {
            let _ = tx.send(AppEvent::Book(b)).await;
        }
    });
    btx
}

fn spawn_ticker(tx: mpsc::Sender<AppEvent>) {
    tokio::spawn(async move {
        let mut iv = tokio::time::interval(Duration::from_secs(1));
        iv.tick().await;
        loop {
            iv.tick().await;
            if tx.send(AppEvent::Tick).await.is_err() { break; }
        }
    });
}

fn spawn_key_reader(tx: mpsc::Sender<AppEvent>) {
    tokio::spawn(async move {
        let mut stream = EventStream::new();
        while let Some(Ok(ev)) = stream.next().await {
            match ev {
                CtEvent::Key(k) if k.kind == crossterm::event::KeyEventKind::Press => {
                    if tx.send(AppEvent::Key(k)).await.is_err() { break; }
                }
                CtEvent::Resize(_, _) => {
                    let _ = tx.send(AppEvent::Tick).await; // force redraw
                }
                _ => {}
            }
        }
    });
}

fn spawn_market_watcher(
    tx:        mpsc::Sender<AppEvent>,
    market_tx: mpsc::Sender<gamma::ActiveMarket>,
) {
    tokio::spawn(async move {
        let gamma = gamma::GammaClient::new();
        let mut current_slug: Option<String> = None;
        loop {
            match gamma.find_current_btc_5m().await {
                Ok(m) => {
                    if current_slug.as_deref() != Some(&m.slug) {
                        current_slug = Some(m.slug.clone());
                        if market_tx.send(m).await.is_err() { break; }
                    }
                }
                Err(e) => {
                    let _ = tx.send(AppEvent::OrderErr(format!("gamma: {e}"))).await;
                }
            }
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
    });
}

// ── action dispatch ─────────────────────────────────────────────────

fn dispatch_action(
    action:  Action,
    state:   &AppState,
    trading: &Arc<Mutex<TradingClient>>,
    tx:      &mpsc::Sender<AppEvent>,
) {
    match action {
        Action::None             => {}
        Action::Quit             => { let _ = tx.try_send(AppEvent::Quit); }
        Action::ForceMarketRoll  => { /* market watcher polls every 10s; r is a no-op for now */ }
        Action::CancelAll        => {
            let t  = trading.clone();
            let tx = tx.clone();
            tokio::spawn(async move {
                match t.lock().await.cancel_all().await {
                    Ok(_)  => { let _ = tx.send(AppEvent::OrderErr("all open orders cancelled".into())).await; }
                    Err(e) => { let _ = tx.send(AppEvent::OrderErr(format!("cancel failed: {e}"))).await; }
                }
            });
        }
        Action::PlaceMarket { outcome, side, size_usdc } => {
            let Some(market) = state.market.clone() else {
                let _ = tx.try_send(AppEvent::OrderErr("no active market".into()));
                return;
            };
            let Some((shares, price, otype)) = resolve_market_order(state, outcome, side, size_usdc) else {
                let _ = tx.try_send(AppEvent::OrderErr("no book liquidity".into()));
                return;
            };
            spawn_order(
                trading.clone(), tx.clone(), market, outcome, side, shares, price, otype,
            );
        }
        Action::PlaceLimit { outcome, side, price, size_usdc } => {
            let Some(market) = state.market.clone() else {
                let _ = tx.try_send(AppEvent::OrderErr("no active market".into()));
                return;
            };
            // For limit buys, `size_usdc` is notional; we convert to shares at the limit price.
            // For limit sells, we treat `size_usdc` as the USDC value at the limit price.
            let shares = (size_usdc / price).max(0.01);
            spawn_order(
                trading.clone(), tx.clone(), market, outcome, side, shares, price, OrderType::Gtc,
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_order(
    trading:  Arc<Mutex<TradingClient>>,
    tx:       mpsc::Sender<AppEvent>,
    market:   gamma::ActiveMarket,
    outcome:  Outcome,
    side:     Side,
    shares:   f64,
    price:    f64,
    otype:    OrderType,
) {
    tokio::spawn(async move {
        let token_id = match outcome {
            Outcome::Up   => market.up_token_id,
            Outcome::Down => market.down_token_id,
        };
        let args = OrderArgs {
            token_id, side, price, size: shares, neg_risk: market.neg_risk,
        };
        let mut cli = trading.lock().await;
        match cli.place_order(args, otype).await {
            Ok(resp) => {
                if resp.success || resp.status.as_deref() == Some("matched") {
                    let _ = tx.send(AppEvent::OrderAck {
                        side, outcome, qty: shares, price,
                    }).await;
                } else {
                    let msg = resp.error.unwrap_or_else(|| format!("status={:?}", resp.status));
                    let _ = tx.send(AppEvent::OrderErr(msg)).await;
                }
            }
            Err(e) => { let _ = tx.send(AppEvent::OrderErr(e.to_string())).await; }
        }
    });
}
