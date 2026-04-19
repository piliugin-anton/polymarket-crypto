//! Polymarket Data API — read-only user positions (`https://data-api.polymarket.com`).
//!
//! Claimable USDC from resolved markets is the sum of `currentValue` on positions with `redeemable: true`.
//! Redeeming on-chain uses CTF `redeemPositions` (see Polymarket CTF docs); typical accounts hold tokens in a
//! Safe — use the web Portfolio **Claim** flow or `polymarket ctf redeem` (official CLI).

use alloy_primitives::Address;
use anyhow::{Context, Result};
use serde::Deserialize;

pub const DATA_API_HOST: &str = "https://data-api.polymarket.com";

#[derive(Debug, Clone, Deserialize)]
pub struct DataPosition {
    #[serde(rename = "conditionId")]
    pub condition_id: String,
    #[serde(default)]
    pub redeemable: bool,
    /// Approximate USDC value of this position in the API (use for claimable estimate).
    #[serde(rename = "currentValue")]
    pub current_value: f64,
    #[serde(default)]
    #[allow(dead_code)]
    pub title: String,
    /// Outcome ERC-1155 token id (decimal or 0x hex).
    #[serde(default)]
    pub asset: String,
    #[serde(rename = "outcomeIndex", default)]
    pub outcome_index: u32,
    #[serde(rename = "negativeRisk", default)]
    pub negative_risk: bool,
}

const POSITIONS_PAGE: u32 = 500;
/// Data API caps `offset` at 10_000 (`offset` + page must stay within spec).
const POSITIONS_MAX_OFFSET: u32 = 10_000;

/// `GET /positions?user=0x...&redeemable=true` with pagination.
///
/// Without `redeemable=true`, the default sort (`TOKENS` DESC) returns only the first `limit`
/// positions — redeemable (often small) rows can fall **past** that window, so claimable sums to 0
/// while the site still shows claimable (it queries redeemable positions).
pub async fn fetch_redeemable_positions(
    http: &reqwest::Client,
    user: Address,
) -> Result<Vec<DataPosition>> {
    let mut out = Vec::new();
    let mut offset: u32 = 0;
    loop {
        // API default `sizeThreshold` is 1; omitting it drops positions with size < 1 (site/scripts use 0).
        let url = format!(
            "{DATA_API_HOST}/positions?user={}&limit={POSITIONS_PAGE}&offset={offset}&redeemable=true&sizeThreshold=0",
            format!("{user:#x}")
        );
        let resp = http.get(&url).send().await.with_context(|| format!("GET {url}"))?;
        let status = resp.status();
        let txt = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("data-api GET /positions failed: {} — {}", status, txt.trim());
        }
        let batch: Vec<DataPosition> = serde_json::from_str(&txt)
            .with_context(|| format!("decode /positions: {}", txt.trim()))?;
        let n = batch.len();
        out.extend(batch);
        if n < POSITIONS_PAGE as usize {
            break;
        }
        offset = offset.saturating_add(POSITIONS_PAGE);
        if offset > POSITIONS_MAX_OFFSET {
            break;
        }
    }
    Ok(out)
}

pub fn sum_claimable_usdc(positions: &[DataPosition]) -> f64 {
    positions
        .iter()
        .filter(|p| p.redeemable)
        .map(|p| p.current_value)
        .filter(|v| v.is_finite())
        .sum()
}
