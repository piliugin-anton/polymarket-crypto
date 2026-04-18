//! Gamma API client.
//!
//! Used exclusively for market *discovery*: we hit this no-auth REST API every
//! ~30s to find the currently-active "Bitcoin Up or Down - 5m" market and extract
//! the UP / DOWN token IDs, the tick size, neg_risk flag, and the "Price to Beat".
//!
//! Market slug pattern:
//!   btc-updown-5m-<unix-timestamp-of-window-start>
//!
//! Each market has a single event with two outcome tokens (Up / Down).

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::config::GAMMA_HOST;

/// What the rest of the app needs to know about a market.
#[derive(Debug, Clone)]
pub struct ActiveMarket {
    pub condition_id:   String,
    pub question:       String,
    pub slug:           String,
    pub up_token_id:    String,
    pub down_token_id:  String,
    pub tick_size:      String,   // e.g. "0.01"
    pub neg_risk:       bool,
    pub price_to_beat:  Option<f64>, // parsed from the rules/description
    pub opens_at:       DateTime<Utc>,
    pub closes_at:      DateTime<Utc>,
}

// ── Raw Gamma JSON shapes ───────────────────────────────────────────
// The Gamma API returns fields in mixed camelCase / snake_case; we
// deserialise with serde(rename_all) and keep only what we actually use.

#[derive(Debug, Deserialize)]
struct RawEvent {
    #[serde(default)] title:     String,
    #[serde(default)] slug:      String,
    #[serde(default, rename = "startDate")] start_date: Option<String>,
    #[serde(default, rename = "endDate")]   end_date:   Option<String>,
    #[serde(default)] markets:   Vec<RawMarket>,
}

#[derive(Debug, Deserialize)]
struct RawMarket {
    #[serde(rename = "conditionId")] condition_id: String,
    #[serde(default)] question: String,
    #[serde(default)] slug:     String,
    /// Newline-separated JSON array `["tokenIdUp","tokenIdDown"]`.
    #[serde(default, rename = "clobTokenIds")] clob_token_ids: String,
    #[serde(default)] outcomes:       String, // e.g. `["Up","Down"]`
    #[serde(default, rename = "orderPriceMinTickSize")] tick_size_f: Option<f64>,
    #[serde(default, rename = "negRisk")] neg_risk: bool,
    #[serde(default)] description:    String,
    #[serde(default, rename = "startDate")] start_date: Option<String>,
    #[serde(default, rename = "endDate")]   end_date:   Option<String>,
    #[serde(default)] closed: bool,
}

pub struct GammaClient {
    http: reqwest::Client,
}

impl GammaClient {
    pub fn new() -> Self {
        Self {
            http: crate::net::reqwest_client().expect("reqwest client"),
        }
    }

    /// Find the 5-min BTC market whose trading window contains `now`.
    ///
    /// Strategy: list recent `btc-updown-5m-*` events, pick the one where
    /// `start_date <= now < end_date`. Falls back to the nearest upcoming one.
    pub async fn find_current_btc_5m(&self) -> Result<ActiveMarket> {
        // The Gamma /events endpoint accepts a `slug` *prefix* via `slug=` +
        // a tag filter; the most reliable approach is actually to query the
        // `crypto` tag filtered for 5-minute BTC.
        //
        // We hit `/events` with the series filter and pick by timestamp.
        let url = format!(
            "{GAMMA_HOST}/events?tag_slug=crypto-5m&active=true&closed=false&limit=50&order=startDate"
        );

        let resp: Vec<RawEvent> = self
            .http
            .get(&url)
            .send()
            .await
            .context("gamma /events request failed")?
            .error_for_status()?
            .json()
            .await
            .context("gamma /events decode failed")?;

        let now = Utc::now();

        // Filter to BTC updown events
        let mut candidates: Vec<_> = resp
            .into_iter()
            .filter(|e| e.slug.starts_with("btc-updown-5m-"))
            .collect();

        // Sort by start date
        candidates.sort_by_key(|e| e.start_date.clone().unwrap_or_default());

        let event = candidates
            .iter()
            .find(|e| {
                match (parse_ts(e.start_date.as_deref()), parse_ts(e.end_date.as_deref())) {
                    (Some(s), Some(c)) => s <= now && now < c,
                    _ => false,
                }
            })
            // Fall back to the next upcoming one
            .or_else(|| {
                candidates.iter().find(|e| {
                    parse_ts(e.start_date.as_deref()).map_or(false, |s| s >= now)
                })
            })
            .ok_or_else(|| anyhow!("no active or upcoming btc-updown-5m-* event found"))?;

        let m = event
            .markets
            .iter()
            .find(|m| !m.closed)
            .ok_or_else(|| anyhow!("event has no open markets"))?;

        // Parse the two token ids out of the JSON-encoded-string field
        let token_ids: Vec<String> = serde_json::from_str(&m.clob_token_ids)
            .context("parsing clobTokenIds")?;
        if token_ids.len() < 2 {
            return Err(anyhow!("expected 2 token ids, got {}", token_ids.len()));
        }
        let outcomes: Vec<String> = serde_json::from_str(&m.outcomes)
            .unwrap_or_else(|_| vec!["Up".into(), "Down".into()]);

        // First outcome is usually "Up" but we match explicitly just in case.
        let (up_token_id, down_token_id) = if outcomes
            .first()
            .map(|s| s.eq_ignore_ascii_case("up"))
            .unwrap_or(true)
        {
            (token_ids[0].clone(), token_ids[1].clone())
        } else {
            (token_ids[1].clone(), token_ids[0].clone())
        };

        let price_to_beat = extract_price_to_beat(&m.description);
        let tick_size = m
            .tick_size_f
            .map(|v| format!("{v:.2}"))
            .unwrap_or_else(|| "0.01".into());

        Ok(ActiveMarket {
            condition_id:   m.condition_id.clone(),
            question:       m.question.clone(),
            slug:           m.slug.clone(),
            up_token_id,
            down_token_id,
            tick_size,
            neg_risk:       m.neg_risk,
            price_to_beat,
            opens_at:  parse_ts(m.start_date.as_deref().or(event.start_date.as_deref()))
                .unwrap_or_else(Utc::now),
            closes_at: parse_ts(m.end_date.as_deref().or(event.end_date.as_deref()))
                .unwrap_or_else(Utc::now),
        })
    }
}

impl Default for GammaClient {
    fn default() -> Self { Self::new() }
}

fn parse_ts(s: Option<&str>) -> Option<DateTime<Utc>> {
    s.and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&Utc))
}

/// Pull "Price to Beat" out of the market description text. The resolution
/// text typically contains something like:
///
///   "The opening BTC price for this window is $67,234.50."
///
/// If the bot can't parse it, we set `price_to_beat = None` and the header
/// colour logic will fall back to comparing against the market implied
/// probability (YES price vs 0.50).
fn extract_price_to_beat(desc: &str) -> Option<f64> {
    // Very loose extraction: find "$NNNN(.NN)"  near the word "price".
    let lower = desc.to_ascii_lowercase();
    let anchor = lower.find("price to beat").or_else(|| lower.find("opening"))?;
    let tail = &desc[anchor..];
    let dollar = tail.find('$')?;
    let rest = &tail[dollar + 1..];
    let num: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == ',')
        .collect();
    num.replace(',', "").parse().ok()
}
