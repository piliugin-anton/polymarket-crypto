//! BTC 5m market discovery via Polymarket CLOB `market` WebSocket.
//!
//! **`new_market` requires a broad subscription.** Community and Polymarket examples use
//! `assets_ids: []` + `custom_feature_enabled: true` to receive global `new_market` events
//! ([`rs-clob-client` #226](https://github.com/Polymarket/rs-clob-client/issues/226)).
//! Subscribing only to the *current* pair of outcome token IDs typically **does not** deliver
//! `new_market` for the *next* window (those markets have different token IDs).
//!
//! Trade-off: the socket receives high-volume `book` / `price_change` traffic for many markets;
//! we only fully parse frames that contain `new_market`. Order books for trading stay on the
//! separate [`crate::feeds::clob_ws`] connection.
//!
//! On a relevant `new_market` (or on the fallback interval), we call
//! [`GammaClient::find_current_btc_5m`] — same as a periodic poller.

use anyhow::{Context, Result};
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tracing::{info, warn};

use crate::app::AppEvent;
use crate::config::CLOB_WS_URL;
use crate::gamma::{self, GammaClient, BTC_5M_WINDOW_SEC};

/// How often to re-check Gamma if the WebSocket delivers no timely `new_market`.
const GAMMA_FALLBACK_SECS: u64 = 60;

fn snip(txt: &str, max: usize) -> String {
    let t = txt.trim();
    if t.len() <= max {
        t.to_string()
    } else {
        format!("{}…", &t[..max])
    }
}

fn slug_might_be_current_btc_5m(slug: &str, now_ts: i64) -> bool {
    let Some(tail) = slug.strip_prefix("btc-updown-5m-") else {
        return false;
    };
    let Ok(window_start) = tail.parse::<i64>() else {
        return false;
    };
    let grid = (now_ts / BTC_5M_WINDOW_SEC) * BTC_5M_WINDOW_SEC;
    (window_start - grid).abs() <= 2 * BTC_5M_WINDOW_SEC
}

fn collect_new_market_slugs_from_value(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::Array(items) => {
            for x in items {
                collect_new_market_slugs_from_value(x, out);
            }
        }
        Value::Object(map) => {
            if map
                .get("event_type")
                .and_then(|t| t.as_str())
                .is_some_and(|t| t == "new_market")
            {
                if let Some(Value::String(slug)) = map.get("slug") {
                    out.push(slug.clone());
                }
            }
        }
        _ => {}
    }
}

async fn apply_resolved_market(
    m:            gamma::ActiveMarket,
    current_slug: &mut Option<String>,
    market_tx:    &mpsc::Sender<gamma::ActiveMarket>,
) -> bool {
    if current_slug.as_deref() == Some(m.slug.as_str()) {
        return true;
    }
    *current_slug = Some(m.slug.clone());
    if market_tx.send(m).await.is_err() {
        warn!("market channel closed — discovery task stopping");
        return false;
    }
    true
}

async fn try_roll_market(
    gamma:        &GammaClient,
    tx:           &mpsc::Sender<AppEvent>,
    market_tx:    &mpsc::Sender<gamma::ActiveMarket>,
    current_slug: &mut Option<String>,
) -> bool {
    match gamma.find_current_btc_5m().await {
        Ok(m) => apply_resolved_market(m, current_slug, market_tx).await,
        Err(e) => {
            let _ = tx
                .send(AppEvent::OrderErr(format!("gamma: {e}")))
                .await;
            true
        }
    }
}

async fn run_session(
    gamma:        &GammaClient,
    tx:           &mpsc::Sender<AppEvent>,
    market_tx:    &mpsc::Sender<gamma::ActiveMarket>,
    current_slug: &mut Option<String>,
) -> Result<bool> {
    let (mut ws, _) = crate::net::ws_connect(CLOB_WS_URL)
        .await
        .context("connect to Polymarket market WS (discovery)")?;

    info!(
        url = %CLOB_WS_URL,
        "market discovery WS connected (assets_ids: [] for global new_market — see module docs)"
    );

    // Broad subscription required for `new_market` (scoped token list filters out other markets).
    let sub = serde_json::json!({
        "type": "market",
        "assets_ids": [],
        "custom_feature_enabled": true,
    });
    ws.send(Message::Text(sub.to_string().into())).await?;

    let mut ping_iv = tokio::time::interval(Duration::from_secs(10));
    ping_iv.tick().await;
    let mut fallback_iv = tokio::time::interval(Duration::from_secs(GAMMA_FALLBACK_SECS));
    fallback_iv.tick().await;

    let mut first_text = true;

    loop {
        tokio::select! {
            _ = ping_iv.tick() => {
                let _ = ws.send(Message::Text("PING".into())).await;
            }
            _ = fallback_iv.tick() => {
                if !try_roll_market(gamma, tx, market_tx, current_slug).await {
                    return Ok(false);
                }
            }
            m = ws.next() => {
                let m = match m {
                    Some(Ok(m)) => m,
                    Some(Err(e)) => return Err(e.into()),
                    None => return Ok(true),
                };
                let txt = match m {
                    Message::Text(t) => t,
                    Message::Close(f) => {
                        info!(?f, "market discovery WS peer closed");
                        return Ok(true);
                    }
                    _ => continue,
                };
                if txt.trim() == "PONG" {
                    continue;
                }
                if !txt.contains("new_market") {
                    continue;
                }
                if first_text {
                    info!(
                        len = txt.len(),
                        snippet = %snip(&txt, 200),
                        "market discovery WS first candidate new_market frame"
                    );
                    first_text = false;
                }

                let v: Value = match serde_json::from_str(&txt) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let now_ts = Utc::now().timestamp();
                let mut slugs = Vec::new();
                collect_new_market_slugs_from_value(&v, &mut slugs);
                let relevant = slugs
                    .iter()
                    .any(|s| slug_might_be_current_btc_5m(s, now_ts));
                if !relevant {
                    continue;
                }

                if !try_roll_market(gamma, tx, market_tx, current_slug).await {
                    return Ok(false);
                }
            }
        }
    }
}

pub fn spawn(tx: mpsc::Sender<AppEvent>, market_tx: mpsc::Sender<gamma::ActiveMarket>) {
    tokio::spawn(async move {
        let gamma = GammaClient::new();
        let mut current_slug: Option<String> = None;
        if !try_roll_market(&gamma, &tx, &market_tx, &mut current_slug).await {
            return;
        }

        loop {
            match run_session(&gamma, &tx, &market_tx, &mut current_slug).await {
                Ok(false) => return,
                Ok(true) => {
                    warn!("market discovery WS session ended — reconnecting in 2s");
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
                Err(e) => {
                    warn!(error = %e, "market discovery WS error — reconnecting in 2s");
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
        }
    });
}
