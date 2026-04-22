//! BTC 5m market discovery via **Gamma REST** (`find_current_btc_5m`).
//!
//! While the current 5m window is open we sleep at most [`GAMMA_POLL_IN_WINDOW_SECS`] (15s),
//! but we also **wake at `closes_at`** (`min` of the two) so the next tick runs as soon as the window ends,
//! with **no** extra post-close delay before the slug poll. **After `closes_at`** we only hit
//! **`GET /markets/slug/btc-updown-5m-{previous_closes_at_unix}`** (the next 5m window, +300s from
//! the last start) once per second until that slug returns 200 — no multi-slug search (faster
//! than [`GammaClient::find_current_btc_5m`]). Then we return to the slow in-window poll.
//! [`GammaClient`] is behind [`tokio::sync::Mutex`] so only one Gamma request runs at a time.
//!
//! The per-market order book still uses [`crate::feeds::clob_ws`] on a separate connection.

use std::sync::Arc;
use std::time::Duration;
use chrono::{DateTime, Utc};
use tokio::sync::{mpsc, Mutex};
use tracing::{info, warn};

use crate::app::AppEvent;
use crate::gamma::{self, GammaClient};

/// Seconds between Gamma discovery ticks while the current 5m window is open.
/// `find_current_btc_5m` does up to 5 GETs per tick — keep this well above 1s to avoid
/// hammering Gamma; 15s is ~4× more responsive than the old 60s default.
pub const GAMMA_POLL_IN_WINDOW_SECS: u64 = 15;

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
    gamma:          &Mutex<GammaClient>,
    tx:             &mpsc::Sender<AppEvent>,
    market_tx:      &mpsc::Sender<gamma::ActiveMarket>,
    current_slug:   &mut Option<String>,
    last_window_end: &mut Option<DateTime<Utc>>,
) -> bool {
    let after_close = last_window_end.is_some_and(|end| Utc::now() >= end);

    // After the window ends, the next market always uses slug start = previous `closes_at`
    // (5m step). Poll that single slug every second — avoids slow multi-candidate `find_current_btc_5m`.
    if after_close {
        if let Some(prev_end) = *last_window_end {
            let next_window_start_ts = prev_end.timestamp();
            loop {
                let result = {
                    let client = gamma.lock().await;
                    client
                        .try_fetch_btc_5m_by_window_start_ts(next_window_start_ts)
                        .await
                };

                match result {
                    Ok(Some(m)) => {
                        *last_window_end = Some(m.closes_at);
                        return apply_resolved_market(m, current_slug, market_tx).await;
                    }
                    Ok(None) => {
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                    Err(e) => {
                        let _ = tx
                            .send(AppEvent::OrderErr(format!("gamma: {e}")))
                            .await;
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                }
            }
        }
    }

    let result = {
        let client = gamma.lock().await;
        client.find_current_btc_5m().await
    };

    match result {
        Ok(m) => {
            *last_window_end = Some(m.closes_at);
            apply_resolved_market(m, current_slug, market_tx).await
        }
        Err(e) => {
            let _ = tx
                .send(AppEvent::OrderErr(format!("gamma: {e}")))
                .await;
            true
        }
    }
}

fn poll_delay_after_tick(
    last_window_end: Option<DateTime<Utc>>,
    in_window_secs: u64,
) -> Duration {
    let now = Utc::now();
    let Some(end) = last_window_end else {
        return Duration::from_secs(in_window_secs);
    };
    if now >= end {
        // Window already ended — do not wait before the next `try_roll` (1 Hz slug wait is inside it).
        return Duration::ZERO;
    }
    let until_close = (end - now)
        .to_std()
        .unwrap_or(Duration::ZERO);
    let regular = Duration::from_secs(in_window_secs);
    std::cmp::min(until_close, regular)
}

pub fn spawn(tx: mpsc::Sender<AppEvent>, market_tx: mpsc::Sender<gamma::ActiveMarket>) {
    tokio::spawn(async move {
        let gamma = Arc::new(Mutex::new(GammaClient::new()));
        let mut current_slug: Option<String> = None;
        let mut last_window_end: Option<DateTime<Utc>> = None;

        let in_window_secs = GAMMA_POLL_IN_WINDOW_SECS;
        info!(
            in_window_sec = in_window_secs,
            "market discovery: Gamma poll (mutex-serialized; sleeps until min(in_window, until closes_at), then slug poll)"
        );

        if !try_roll_market(
            &gamma,
            &tx,
            &market_tx,
            &mut current_slug,
            &mut last_window_end,
        )
        .await
        {
            return;
        }

        loop {
            tokio::time::sleep(poll_delay_after_tick(last_window_end, in_window_secs)).await;
            if !try_roll_market(
                &gamma,
                &tx,
                &market_tx,
                &mut current_slug,
                &mut last_window_end,
            )
            .await
            {
                return;
            }
        }
    });
}
