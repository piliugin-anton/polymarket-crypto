//! Polymarket CLOB **user** WebSocket (`/ws/user`) — authenticated `trade` / `order` events.
//!
//! Subscription uses `auth` + `markets` (condition IDs). See
//! <https://docs.polymarket.com/market-data/websocket/user-channel.md>.
//!
//! After the initial authenticated subscribe, **market changes** use dynamic
//! `{"markets":[…], "operation":"subscribe"|"unsubscribe"}` so the socket stays open.
//!
//! **`trade`** payloads are forwarded to [`crate::trading::FillWaitRegistry`] so take-profit can
//! refine size from the push stream (often ahead of `GET /data/trades` indexing).

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::sync::watch;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info, warn};

use crate::config::CLOB_WS_USER_URL;
use crate::net;
use crate::trading::{ApiCreds, FillWaitRegistry, TradingClient};

type UserWsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

pub fn spawn(
    fill_registry: Arc<FillWaitRegistry>,
    trading: Arc<TradingClient>,
    mut market_watch: watch::Receiver<String>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let creds = match trading.ensure_creds().await {
                Ok(c) => c,
                Err(e) => {
                    warn!(error = %e, "CLOB user WS: cannot ensure API creds — retry in 5s");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    continue;
                }
            };
            let (mut ws, _) = match net::ws_connect(CLOB_WS_USER_URL).await {
                Ok(p) => p,
                Err(e) => {
                    warn!(error = %e, "CLOB user WS connect failed — retry in 2s");
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    continue;
                }
            };

            info!(url = %CLOB_WS_USER_URL, "CLOB user WS connected (session)");

            if let Err(e) = run_session(
                &mut ws,
                &creds,
                &fill_registry,
                &mut market_watch,
            )
            .await
            {
                warn!(error = %e, "CLOB user WS session ended — reconnecting in 2s");
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    })
}

async fn wait_nonempty_market(rx: &mut watch::Receiver<String>) -> Result<String> {
    loop {
        let v = rx.borrow().clone();
        if !v.is_empty() {
            return Ok(v);
        }
        rx.changed()
            .await
            .map_err(|_| anyhow::anyhow!("user WS: market watch sender dropped"))?;
    }
}

async fn send_initial_user_sub(
    ws: &mut UserWsStream,
    creds: &ApiCreds,
    condition_id: &str,
) -> Result<()> {
    let sub = serde_json::json!({
        "type": "user",
        "markets": [condition_id],
        "auth": {
            "apiKey": creds.api_key,
            "secret": creds.secret,
            "passphrase": creds.passphrase,
        },
    });
    ws.send(Message::Text(sub.to_string().into()))
        .await
        .context("user WS initial subscribe send")?;
    Ok(())
}

async fn send_market_operation(
    ws: &mut UserWsStream,
    condition_id: &str,
    operation: &str,
) -> Result<()> {
    let msg = serde_json::json!({
        "markets": [condition_id],
        "operation": operation,
    });
    ws.send(Message::Text(msg.to_string().into()))
        .await
        .with_context(|| format!("user WS {operation} send"))?;
    Ok(())
}

async fn run_session(
    ws: &mut UserWsStream,
    creds: &ApiCreds,
    registry: &FillWaitRegistry,
    market_watch: &mut watch::Receiver<String>,
) -> Result<()> {
    let mut current_sub: Option<String> = None;
    let mut ping = tokio::time::interval(Duration::from_secs(10));
    ping.tick().await;

    let mut logged_first = false;

    loop {
        let desired = wait_nonempty_market(market_watch).await?;
        if current_sub.as_deref() != Some(desired.as_str()) {
            if let Some(ref old_id) = current_sub {
                send_market_operation(ws, old_id, "unsubscribe").await?;
            }
            if current_sub.is_none() {
                send_initial_user_sub(ws, creds, &desired).await?;
            } else {
                send_market_operation(ws, &desired, "subscribe").await?;
            }
            info!(
                market = %desired,
                prev = ?current_sub.as_deref(),
                "CLOB user WS market filter updated (same connection)"
            );
            current_sub = Some(desired);
        }

        tokio::select! {
            r = market_watch.changed() => {
                r.map_err(|_| anyhow::anyhow!("user WS: market watch closed"))?;
                continue;
            }
            _ = ping.tick() => {
                let _ = ws.send(Message::Text("PING".into())).await;
            }
            m = ws.next() => {
                let m = match m {
                    Some(Ok(m)) => m,
                    Some(Err(e)) => return Err(e.into()),
                    None => {
                        info!("CLOB user WS stream ended");
                        return Ok(());
                    }
                };
                let txt = match m {
                    Message::Text(t) => t.to_string(),
                    Message::Close(f) => {
                        info!(?f, "CLOB user WS peer closed");
                        return Ok(());
                    }
                    _ => continue,
                };
                if txt.trim().eq_ignore_ascii_case("PONG") {
                    continue;
                }
                if !logged_first {
                    let snip: String = txt.chars().take(220).collect();
                    debug!(len = txt.len(), %snip, "CLOB user WS first payload");
                    logged_first = true;
                }
                registry.dispatch_from_ws_text(&txt).await;
            }
        }
    }
}
