//! Polymarket RTDS — Chainlink BTC/USD feed.
//!
//! No authentication required. We subscribe to the `crypto_prices_chainlink`
//! topic with `symbol=btc/usd` and emit a `PriceTick` on every update.
//!
//! The public doc is at <https://docs.polymarket.com/developers/RTDS/RTDS-crypto-prices>.

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, warn};

use crate::config::RTDS_WS_URL;

#[derive(Debug, Clone, Copy)]
pub struct PriceTick {
    pub price:        f64,
    pub timestamp_ms: u64,
}

#[derive(Debug, Deserialize)]
struct Envelope {
    #[serde(default)] topic: String,
    #[serde(default, rename = "type")] kind: String,
    #[serde(default)] payload: Option<Payload>,
}

#[derive(Debug, Deserialize)]
struct Payload {
    #[serde(default)] symbol:    String,
    #[serde(default)] value:     Option<f64>,
    #[serde(default)] timestamp: Option<u64>,
    /// For "subscribe" envelopes the payload is a batch of points.
    #[serde(default)] data:      Vec<DataPoint>,
}

#[derive(Debug, Deserialize)]
struct DataPoint {
    #[serde(default)] value:     f64,
    #[serde(default)] timestamp: u64,
}

/// Spawns a task that maintains the WS connection with reconnect-on-error,
/// pushing every new price tick into `tx`.
pub fn spawn(tx: mpsc::Sender<PriceTick>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if let Err(e) = run_once(&tx).await {
                warn!(error = %e, "chainlink WS disconnected — reconnecting in 2s");
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        }
    })
}

async fn run_once(tx: &mpsc::Sender<PriceTick>) -> Result<()> {
    let (mut ws, _) = crate::net::ws_connect(RTDS_WS_URL)
        .await
        .context("connect to Polymarket RTDS")?;

    // Subscribe to the Chainlink crypto topic.
    let sub = serde_json::json!({
        "action":     "subscribe",
        "topic":      "crypto_prices_chainlink",
        "type":       "*",
        "filters":    { "symbol": "btc/usd" },
    });
    ws.send(Message::Text(sub.to_string().into())).await?;

    // Heartbeat loop — server closes idle connections after ~5s.
    let mut ping_iv = tokio::time::interval(std::time::Duration::from_secs(5));
    ping_iv.tick().await; // consume the immediate first tick

    loop {
        tokio::select! {
            _ = ping_iv.tick() => {
                let _ = ws.send(Message::Text("PING".into())).await;
            }
            msg = ws.next() => {
                let msg = match msg {
                    Some(Ok(m)) => m,
                    Some(Err(e)) => return Err(e.into()),
                    None => return Ok(()),
                };
                match msg {
                    Message::Text(txt) => {
                        // Server sends "PONG" as plain text — ignore.
                        if txt.trim() == "PONG" { continue; }
                        match serde_json::from_str::<Envelope>(&txt) {
                            Ok(env) if env.topic == "crypto_prices_chainlink" => {
                                if let Some(p) = env.payload {
                                    // Live update
                                    if let (Some(v), Some(ts)) = (p.value, p.timestamp) {
                                        let _ = tx.send(PriceTick { price: v, timestamp_ms: ts }).await;
                                    }
                                    // Snapshot catch-up on subscribe
                                    if let Some(last) = p.data.last() {
                                        let _ = tx.send(PriceTick {
                                            price: last.value,
                                            timestamp_ms: last.timestamp,
                                        }).await;
                                    }
                                }
                            }
                            Ok(_) => {}
                            Err(e) => debug!(%e, raw = %txt, "failed to parse RTDS message"),
                        }
                    }
                    Message::Close(_) => return Ok(()),
                    _ => {}
                }
            }
        }
    }
}
