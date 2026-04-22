//! Rolling / daily market discovery via **Gamma REST** ([`crate::gamma::GammaClient::find_current_updown`]).
//!
//! While the current window is open we sleep at most [`GAMMA_POLL_IN_WINDOW_SECS`], but we also
//! **wake at `closes_at`** (`min` of the two) so the next tick runs as soon as the window ends.
//! **After `closes_at`**: for **rolling** markets, poll **`GET /markets/slug/…-updown-…-{previous_closes_at_unix}`**
//! once per second until that slug returns 200. For **daily** markets, resolve the next ET calendar
//! day and poll that slug at 1 Hz.
//! [`GammaClient`] is behind [`tokio::sync::Mutex`] so only one Gamma request runs at a time.

use std::sync::Arc;
use std::time::Duration;
use chrono::{DateTime, Utc};
use tokio::sync::{mpsc, Mutex};
use tracing::{info, warn};

use crate::app::AppEvent;
use crate::gamma::{self, GammaClient};
use crate::market_profile::MarketProfile;

/// Seconds between Gamma discovery ticks while the current window is open.
pub const GAMMA_POLL_IN_WINDOW_SECS: u64 = 15;

/// Ignore sub-dollar noise when comparing Polymarket `openPrice` refinements.
const PRICE_TO_BEAT_EPS_USD: f64 = 0.5;

fn price_to_beat_close(a: Option<f64>, b: Option<f64>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(x), Some(y)) if x.is_finite() && y.is_finite() => (x - y).abs() <= PRICE_TO_BEAT_EPS_USD,
        _ => false,
    }
}

async fn apply_resolved_market(
    m:            gamma::ActiveMarket,
    last_emitted: &mut Option<(String, Option<f64>)>,
    market_tx:    &mpsc::Sender<gamma::ActiveMarket>,
    app_tx:       &mpsc::Sender<AppEvent>,
) -> bool {
    let key = (m.slug.clone(), m.price_to_beat);
    let full_roll = match last_emitted {
        None => true,
        Some((s, _)) if s.as_str() != m.slug.as_str() => true,
        _ => false,
    };

    if full_roll {
        *last_emitted = Some(key);
        if market_tx.send(m).await.is_err() {
            warn!("market channel closed — discovery task stopping");
            return false;
        }
        return true;
    }

    if last_emitted
        .as_ref()
        .is_some_and(|(s, p)| s.as_str() == m.slug.as_str() && price_to_beat_close(*p, m.price_to_beat))
    {
        return true;
    }

    let slug = m.slug.clone();
    let ptb = m.price_to_beat;
    *last_emitted = Some((slug.clone(), ptb));
    if app_tx
        .send(AppEvent::PriceToBeatRefresh {
            slug,
            price_to_beat: ptb,
        })
        .await
        .is_err()
    {
        warn!("app event channel closed — discovery task stopping");
        return false;
    }
    true
}

async fn try_roll_market(
    gamma:           &Mutex<GammaClient>,
    tx:              &mpsc::Sender<AppEvent>,
    market_tx:       &mpsc::Sender<gamma::ActiveMarket>,
    last_emitted:    &mut Option<(String, Option<f64>)>,
    last_window_end: &mut Option<DateTime<Utc>>,
    profile:         &MarketProfile,
) -> bool {
    let after_close = last_window_end.is_some_and(|end| Utc::now() >= end);

    if after_close {
        if let Some(prev_end) = *last_window_end {
            if profile.is_rolling() {
                let next_window_start_ts = prev_end.timestamp();
                loop {
                    let result = {
                        let client = gamma.lock().await;
                        client
                            .try_fetch_rolling_by_window_start_ts(next_window_start_ts, profile)
                            .await
                    };

                    match result {
                        Ok(Some(m)) => {
                            *last_window_end = Some(m.closes_at);
                            return apply_resolved_market(m, last_emitted, market_tx, tx).await;
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
            } else {
                loop {
                    let result = {
                        let client = gamma.lock().await;
                        client
                            .try_fetch_next_daily_by_previous_close(profile, prev_end)
                            .await
                    };
                    match result {
                        Ok(Some(m)) => {
                            *last_window_end = Some(m.closes_at);
                            return apply_resolved_market(m, last_emitted, market_tx, tx).await;
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
    }

    let result = {
        let client = gamma.lock().await;
        client.find_current_updown(profile).await
    };

    match result {
        Ok(m) => {
            *last_window_end = Some(m.closes_at);
            apply_resolved_market(m, last_emitted, market_tx, tx).await
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
        return Duration::ZERO;
    }
    let until_close = (end - now)
        .to_std()
        .unwrap_or(Duration::ZERO);
    let regular = Duration::from_secs(in_window_secs);
    std::cmp::min(until_close, regular)
}

/// `profile` is fixed for the process (chosen in the TUI wizard before this task is spawned).
pub fn spawn(
    tx:       mpsc::Sender<AppEvent>,
    market_tx: mpsc::Sender<gamma::ActiveMarket>,
    profile:  Arc<MarketProfile>,
) {
    tokio::spawn(async move {
        let gamma = Arc::new(Mutex::new(GammaClient::new()));
        let mut last_emitted: Option<(String, Option<f64>)> = None;
        let mut last_window_end: Option<DateTime<Utc>> = None;

        let in_window_secs = GAMMA_POLL_IN_WINDOW_SECS;
        info!(
            in_window_sec = in_window_secs,
            ?profile,
            "market discovery: Gamma poll (mutex-serialized; sleeps until min(in_window, until closes_at), then slug poll)"
        );

        if !try_roll_market(
            &gamma,
            &tx,
            &market_tx,
            &mut last_emitted,
            &mut last_window_end,
            &profile,
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
                &mut last_emitted,
                &mut last_window_end,
                &profile,
            )
            .await
            {
                return;
            }
        }
    });
}
