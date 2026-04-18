//! Polymarket CLOB WebSocket — public `market` channel for order book + trades.
//!
//! We subscribe to the two token IDs (UP and DOWN) of the active market and
//! consume book snapshots + price changes. For user-specific fills we'd need
//! the `user` channel with signed auth — left as a follow-up (see trading.rs
//! for how we'd derive the L2 creds).

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tracing::warn;

use crate::config::CLOB_WS_URL;

#[derive(Debug, Clone)]
pub struct BookLevel {
    pub price: f64,
    pub size:  f64,
}

#[derive(Debug, Clone)]
pub struct BookSnapshot {
    pub asset_id: String,
    pub bids:     Vec<BookLevel>, // sorted high → low
    pub asks:     Vec<BookLevel>, // sorted low → high
}

#[derive(Debug, Deserialize)]
#[serde(tag = "event_type")]
enum RawEvent {
    #[serde(rename = "book")]
    Book {
        asset_id: String,
        #[serde(default)] bids: Vec<RawLevel>,
        #[serde(default)] asks: Vec<RawLevel>,
    },
    #[serde(rename = "price_change")]
    PriceChange {
        asset_id: String,
        #[serde(default)] changes: Vec<RawChange>,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
struct RawLevel {
    price: String,
    size:  String,
}

#[derive(Debug, Deserialize)]
struct RawChange {
    price: String,
    side:  String, // "BUY" or "SELL"
    size:  String,
}

pub fn spawn(token_ids: Vec<String>, tx: mpsc::Sender<BookSnapshot>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if let Err(e) = run_once(&token_ids, &tx).await {
                warn!(error = %e, "CLOB WS disconnected — reconnecting in 2s");
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        }
    })
}

async fn run_once(token_ids: &[String], tx: &mpsc::Sender<BookSnapshot>) -> Result<()> {
    let (mut ws, _) = tokio_tungstenite::connect_async(CLOB_WS_URL)
        .await
        .context("connect to Polymarket CLOB WS")?;

    // Market channel subscribe message
    let sub = serde_json::json!({
        "type":          "market",
        "assets_ids":    token_ids,
    });
    ws.send(Message::Text(sub.to_string().into())).await?;

    // Keep local book state so price_change events can be applied
    use std::collections::{BTreeMap, HashMap};
    // asset_id → (bids map, asks map). We use i64 keyed by price*100 because
    // price tick is 0.01 and BTreeMap needs Ord.
    let mut books: HashMap<String, (BTreeMap<i64, f64>, BTreeMap<i64, f64>)> = HashMap::new();

    let mut ping = tokio::time::interval(std::time::Duration::from_secs(10));
    ping.tick().await;

    loop {
        tokio::select! {
            _ = ping.tick() => {
                let _ = ws.send(Message::Text("PING".into())).await;
            }
            m = ws.next() => {
                let m = match m {
                    Some(Ok(m)) => m,
                    Some(Err(e)) => return Err(e.into()),
                    None => return Ok(()),
                };
                let txt = match m {
                    Message::Text(t) => t,
                    Message::Close(_) => return Ok(()),
                    _ => continue,
                };
                if txt.trim() == "PONG" { continue; }

                // CLOB sends arrays of events sometimes; handle both shapes.
                let events: Vec<RawEvent> = match serde_json::from_str::<Vec<RawEvent>>(&txt) {
                    Ok(v) => v,
                    Err(_) => match serde_json::from_str::<RawEvent>(&txt) {
                        Ok(e) => vec![e],
                        Err(_) => continue,
                    }
                };

                for ev in events {
                    match ev {
                        RawEvent::Book { asset_id, bids, asks } => {
                            let entry = books.entry(asset_id.clone()).or_default();
                            entry.0.clear(); entry.1.clear();
                            for l in &bids {
                                if let (Ok(p), Ok(s)) = (l.price.parse::<f64>(), l.size.parse::<f64>()) {
                                    entry.0.insert((p * 100.0).round() as i64, s);
                                }
                            }
                            for l in &asks {
                                if let (Ok(p), Ok(s)) = (l.price.parse::<f64>(), l.size.parse::<f64>()) {
                                    entry.1.insert((p * 100.0).round() as i64, s);
                                }
                            }
                            send_snapshot(&asset_id, entry, tx).await;
                        }
                        RawEvent::PriceChange { asset_id, changes } => {
                            let entry = books.entry(asset_id.clone()).or_default();
                            for c in &changes {
                                let (Ok(p), Ok(s)) = (c.price.parse::<f64>(), c.size.parse::<f64>()) else { continue };
                                let key = (p * 100.0).round() as i64;
                                let map = if c.side == "BUY" { &mut entry.0 } else { &mut entry.1 };
                                if s == 0.0 { map.remove(&key); } else { map.insert(key, s); }
                            }
                            send_snapshot(&asset_id, entry, tx).await;
                        }
                        RawEvent::Other => {}
                    }
                }
            }
        }
    }
}

async fn send_snapshot(
    asset_id: &str,
    (bids, asks): &(std::collections::BTreeMap<i64, f64>, std::collections::BTreeMap<i64, f64>),
    tx: &mpsc::Sender<BookSnapshot>,
) {
    // bids: high→low, asks: low→high
    let bids: Vec<_> = bids.iter().rev().map(|(p, s)| BookLevel { price: *p as f64 / 100.0, size: *s }).collect();
    let asks: Vec<_> = asks.iter().map(|(p, s)| BookLevel { price: *p as f64 / 100.0, size: *s }).collect();
    let _ = tx.send(BookSnapshot { asset_id: asset_id.to_string(), bids, asks }).await;
}
