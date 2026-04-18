//! Central application state + event dispatch.
//!
//! Three async sources push into a single `AppEvent` channel:
//!   1. crossterm key events
//!   2. Chainlink price ticks
//!   3. CLOB book snapshots
//!   4. Periodic ticks for market rolling
//!
//! Trading actions post results back through the same channel.

use chrono::{DateTime, Utc};
use std::collections::VecDeque;

use crate::feeds::{chainlink::PriceTick, clob_ws::BookSnapshot};
use crate::gamma::ActiveMarket;
use crate::trading::{Side, OrderType};

// ── Public event enum ───────────────────────────────────────────────

#[derive(Debug)]
pub enum AppEvent {
    Tick,                         // 1-Hz clock
    Price(PriceTick),
    Book(BookSnapshot),
    MarketRoll(ActiveMarket),
    Key(crossterm::event::KeyEvent),
    OrderAck { side: Side, outcome: Outcome, qty: f64, price: f64 },
    OrderErr(String),
    Quit,
}

// ── UI-level types ──────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome { Up, Down }
impl Outcome {
    pub fn as_str(self) -> &'static str { match self { Outcome::Up => "UP", Outcome::Down => "DOWN" } }
    pub fn opposite(self) -> Self { match self { Outcome::Up => Outcome::Down, Outcome::Down => Outcome::Up } }
}

#[derive(Debug, Clone, Default)]
pub struct Position {
    pub shares:    f64,
    pub avg_entry: f64, // volume-weighted, in price terms (0.01..0.99)
}
impl Position {
    fn add(&mut self, qty: f64, price: f64) {
        let new_total = self.shares + qty;
        if new_total.abs() < 1e-9 { *self = Default::default(); return; }
        self.avg_entry = (self.shares * self.avg_entry + qty * price) / new_total;
        self.shares    = new_total;
    }
    fn reduce(&mut self, qty: f64, price: f64) -> f64 {
        // returns realized pnl for the portion closed
        let closed = qty.min(self.shares);
        let pnl    = closed * (price - self.avg_entry);
        self.shares -= closed;
        if self.shares.abs() < 1e-9 { *self = Default::default(); }
        pnl
    }
}

#[derive(Debug, Clone)]
pub struct Fill {
    pub ts:      DateTime<Utc>,
    pub side:    Side,
    pub outcome: Outcome,
    pub qty:     f64,
    pub price:   f64,
    pub realized: f64, // only non-zero when the fill closes part of a position
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputMode {
    Normal,
    EditSize,
    LimitModal { outcome: Outcome, side: Side, field: LimitField },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitField { Price, Size }

#[derive(Debug, Clone)]
pub struct AppState {
    pub market:         Option<ActiveMarket>,
    pub btc_price:      Option<f64>,
    pub btc_price_ts:   Option<DateTime<Utc>>,

    // One book per outcome token
    pub book_up:        Option<BookSnapshot>,
    pub book_down:      Option<BookSnapshot>,

    pub position_up:    Position,
    pub position_down:  Position,
    pub realized_pnl:   f64,

    pub fills:          VecDeque<Fill>,
    pub status_line:    String,

    pub default_size_usdc: f64,
    pub size_input:        String, // buffer while editing size
    pub limit_price_input: String,
    pub limit_size_input:  String,

    pub input_mode:        InputMode,
}

impl AppState {
    pub fn new(default_size_usdc: f64) -> Self {
        Self {
            market: None, btc_price: None, btc_price_ts: None,
            book_up: None, book_down: None,
            position_up: Default::default(), position_down: Default::default(),
            realized_pnl: 0.0, fills: VecDeque::with_capacity(64),
            status_line: "Waiting for market data…".into(),
            default_size_usdc,
            size_input: format!("{default_size_usdc:.2}"),
            limit_price_input: String::new(),
            limit_size_input:  String::new(),
            input_mode: InputMode::Normal,
        }
    }

    // ── Queries ─────────────────────────────────────────────────────
    pub fn price_to_beat(&self) -> Option<f64> { self.market.as_ref().and_then(|m| m.price_to_beat) }

    /// BTC colour: green when we're at/above the opening price (UP winning),
    /// red when strictly below.
    pub fn btc_above_target(&self) -> Option<bool> {
        match (self.btc_price, self.price_to_beat()) {
            (Some(p), Some(t)) => Some(p >= t),
            _ => None,
        }
    }

    pub fn book_for(&self, outcome: Outcome) -> Option<&BookSnapshot> {
        match outcome { Outcome::Up => self.book_up.as_ref(), Outcome::Down => self.book_down.as_ref() }
    }

    /// Best ask on the outcome side we'd hit when buying YES(outcome).
    pub fn best_ask(&self, outcome: Outcome) -> Option<f64> {
        self.book_for(outcome)?.asks.first().map(|l| l.price)
    }
    /// Best bid on the outcome side we'd hit when selling YES(outcome).
    pub fn best_bid(&self, outcome: Outcome) -> Option<f64> {
        self.book_for(outcome)?.bids.first().map(|l| l.price)
    }

    /// Current mark (mid) price for an outcome, for unrealized PnL.
    pub fn mark(&self, outcome: Outcome) -> Option<f64> {
        let b = self.book_for(outcome)?;
        let bid = b.bids.first().map(|l| l.price);
        let ask = b.asks.first().map(|l| l.price);
        match (bid, ask) {
            (Some(b), Some(a)) => Some((b + a) / 2.0),
            (Some(b), None)    => Some(b),
            (None, Some(a))    => Some(a),
            _                  => None,
        }
    }

    pub fn position(&self, outcome: Outcome) -> &Position {
        match outcome { Outcome::Up => &self.position_up, Outcome::Down => &self.position_down }
    }
    fn position_mut(&mut self, outcome: Outcome) -> &mut Position {
        match outcome { Outcome::Up => &mut self.position_up, Outcome::Down => &mut self.position_down }
    }

    pub fn unrealized_pnl(&self, outcome: Outcome) -> f64 {
        let p = self.position(outcome);
        match self.mark(outcome) {
            Some(m) if p.shares > 0.0 => p.shares * (m - p.avg_entry),
            _ => 0.0,
        }
    }

    pub fn total_pnl(&self) -> f64 {
        self.realized_pnl + self.unrealized_pnl(Outcome::Up) + self.unrealized_pnl(Outcome::Down)
    }

    pub fn countdown_secs(&self) -> Option<i64> {
        self.market.as_ref().map(|m| (m.closes_at - Utc::now()).num_seconds().max(0))
    }

    pub fn current_size(&self) -> f64 {
        self.size_input.parse().unwrap_or(self.default_size_usdc)
    }

    // ── Mutations ───────────────────────────────────────────────────
    pub fn apply(&mut self, ev: AppEvent) {
        match ev {
            AppEvent::Tick => {}
            AppEvent::Price(p) => {
                self.btc_price    = Some(p.price);
                self.btc_price_ts = chrono::DateTime::<Utc>::from_timestamp_millis(p.timestamp_ms as i64);
            }
            AppEvent::Book(b) => {
                if let Some(m) = &self.market {
                    if b.asset_id == m.up_token_id   { self.book_up   = Some(b); }
                    else if b.asset_id == m.down_token_id { self.book_down = Some(b); }
                }
            }
            AppEvent::MarketRoll(m) => {
                // Close any positions from the previous market — they'll resolve
                // via Polymarket and show up as realized once winnings redeem.
                // We keep realized_pnl but zero out live positions for the new market.
                self.status_line = format!("New market: {}", m.question);
                self.market = Some(m);
                self.book_up = None; self.book_down = None;
                self.position_up = Default::default();
                self.position_down = Default::default();
            }
            AppEvent::OrderAck { side, outcome, qty, price } => {
                let realized = match side {
                    Side::Buy  => { self.position_mut(outcome).add(qty, price); 0.0 }
                    Side::Sell => self.position_mut(outcome).reduce(qty, price),
                };
                self.realized_pnl += realized;
                self.fills.push_front(Fill {
                    ts: Utc::now(), side, outcome, qty, price, realized,
                });
                if self.fills.len() > 64 { self.fills.pop_back(); }
                self.status_line = format!("{} {qty:.2} {} @ {price:.3} ✓",
                    side_str(side), outcome.as_str());
            }
            AppEvent::OrderErr(e) => self.status_line = format!("✗ {e}"),
            AppEvent::Key(_) | AppEvent::Quit => {} // handled in events.rs
        }
    }
}

fn side_str(s: Side) -> &'static str { match s { Side::Buy => "BUY", Side::Sell => "SELL" } }

/// Derive the OrderArgs + order_type we'd submit for a given user intent,
/// given the current book.
pub fn resolve_market_order(
    state: &AppState,
    outcome: Outcome,
    side: Side,
    size_usdc_or_shares: f64,
) -> Option<(f64, f64, OrderType)> {
    // We use FOK against the best visible level. If the level has less size
    // than requested, caller should fall back to FAK.
    match side {
        Side::Buy => {
            let ask = state.best_ask(outcome)?;
            // size_usdc_or_shares is interpreted as USDC notional for buys
            let shares = (size_usdc_or_shares / ask).max(0.01);
            Some((shares, ask, OrderType::Fak))
        }
        Side::Sell => {
            let bid = state.best_bid(outcome)?;
            // For sells the size is in shares — but the TUI keeps "size" in USDC,
            // so we convert USDC notional → shares at bid for consistency.
            let shares = (size_usdc_or_shares / bid).max(0.01);
            Some((shares, bid, OrderType::Fak))
        }
    }
}
