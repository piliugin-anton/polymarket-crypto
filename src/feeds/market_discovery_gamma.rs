//! BTC 5m market discovery via **Gamma REST** (`find_current_btc_5m`).
//!
//! While the current 5m window is open we poll slowly; **after `closes_at`** we poll
//! every second until Gamma returns a new window (then back to slow poll).
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

/// Poll interval while the resolved market window is still active.
const GAMMA_POLL_SECS_IN_WINDOW: u64 = 60;
/// Poll interval after `closes_at` until a new market slug appears.
const GAMMA_POLL_SECS_AFTER_WINDOW: u64 = 1;

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

fn poll_delay_after_tick(last_window_end: Option<DateTime<Utc>>) -> Duration {
    let now = Utc::now();
    let after_close = last_window_end.is_some_and(|end| now >= end);
    let secs = if after_close {
        GAMMA_POLL_SECS_AFTER_WINDOW
    } else {
        GAMMA_POLL_SECS_IN_WINDOW
    };
    Duration::from_secs(secs)
}

pub fn spawn(tx: mpsc::Sender<AppEvent>, market_tx: mpsc::Sender<gamma::ActiveMarket>) {
    tokio::spawn(async move {
        let gamma = Arc::new(Mutex::new(GammaClient::new()));
        let mut current_slug: Option<String> = None;
        let mut last_window_end: Option<DateTime<Utc>> = None;

        info!(
            in_window_sec = GAMMA_POLL_SECS_IN_WINDOW,
            after_window_sec = GAMMA_POLL_SECS_AFTER_WINDOW,
            "market discovery: Gamma poll (mutex-serialized requests)"
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
            tokio::time::sleep(poll_delay_after_tick(last_window_end)).await;
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
