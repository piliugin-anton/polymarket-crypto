//! Polymarket relayer v2 — `POST /submit` with **`RELAYER_API_KEY`** + **`RELAYER_API_KEY_ADDRESS`**
//! (Settings → API → Relayer), same as CTF redeem in [`crate::redeem`].

use alloy_primitives::Address;
use anyhow::{bail, Context, Result};
use reqwest::Client;
use serde::Deserialize;
use std::time::Duration;

const SUBMIT_PATH: &str = "/submit";
const GET_TX_PATH: &str = "/transaction";

#[derive(Debug, Deserialize)]
pub struct RelayerSubmitResponse {
    #[serde(rename = "transactionID")]
    pub transaction_id: String,
    pub state: String,
    #[serde(default, rename = "transactionHash")]
    pub transaction_hash: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct RelayerTransactionRow {
    pub state: String,
    #[serde(default, rename = "transactionHash")]
    pub transaction_hash: Option<String>,
}

fn normalize_relayer_base(url: &str) -> String {
    let u = url.trim();
    if u.ends_with('/') {
        u[..u.len() - 1].to_string()
    } else {
        u.to_string()
    }
}

#[derive(serde::Serialize)]
struct WalletCreateBody {
    #[serde(rename = "type")]
    ty: &'static str,
    from: String,
    to: String,
}

fn wallet_create_body(owner: Address, factory: Address) -> WalletCreateBody {
    WalletCreateBody {
        ty: "WALLET-CREATE",
        from: format!("{owner:#x}"),
        to: format!("{factory:#x}"),
    }
}

/// Submit [`WALLET-CREATE`](https://docs.polymarket.com/trading/deposit-wallet-migration).
pub async fn submit_wallet_create(
    http: &Client,
    relayer_base_url: &str,
    owner: Address,
    factory: Address,
    relayer_api_key: &str,
    relayer_api_key_address: Address,
) -> Result<RelayerSubmitResponse> {
    let body = wallet_create_body(owner, factory);
    let url = format!("{}{SUBMIT_PATH}", normalize_relayer_base(relayer_base_url));
    let resp = http
        .post(&url)
        .header("RELAYER_API_KEY", relayer_api_key)
        .header(
            "RELAYER_API_KEY_ADDRESS",
            format!("{relayer_api_key_address:#x}"),
        )
        .json(&body)
        .send()
        .await
        .context("POST relayer /submit")?;
    let status = resp.status();
    let txt = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("relayer POST /submit: HTTP {} — {}", status, txt.trim());
    }
    serde_json::from_str(&txt).with_context(|| format!("decode /submit JSON: {}", txt.trim()))
}

pub async fn get_transaction_rows(
    http: &Client,
    relayer_base_url: &str,
    transaction_id: &str,
) -> Result<Vec<RelayerTransactionRow>> {
    let base = normalize_relayer_base(relayer_base_url);
    let mut u = url::Url::parse(&format!("{base}{GET_TX_PATH}"))
        .with_context(|| format!("invalid relayer URL {base}"))?;
    u.query_pairs_mut().append_pair("id", transaction_id);
    let resp = http
        .get(u.as_str())
        .send()
        .await
        .context("GET relayer /transaction")?;
    let status = resp.status();
    let txt = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("relayer GET /transaction: HTTP {} — {}", status, txt.trim());
    }
    serde_json::from_str(&txt).with_context(|| format!("decode /transaction: {}", txt.trim()))
}

/// Poll until mined/confirmed or failed.
pub async fn wait_relayer_transaction(
    http: &Client,
    relayer_base_url: &str,
    transaction_id: &str,
    max_polls: u32,
    poll_ms: u64,
) -> Result<RelayerTransactionRow> {
    for _ in 0..max_polls {
        let rows = get_transaction_rows(http, relayer_base_url, transaction_id).await?;
        if let Some(txn) = rows.into_iter().next() {
            match txn.state.as_str() {
                "STATE_MINED" | "STATE_CONFIRMED" => return Ok(txn),
                "STATE_FAILED" | "STATE_INVALID" => {
                    bail!("relayer transaction {} failed (state={})", transaction_id, txn.state);
                }
                _ => {}
            }
        }
        tokio::time::sleep(Duration::from_millis(poll_ms)).await;
    }
    bail!(
        "relayer transaction {} did not reach STATE_MINED / STATE_CONFIRMED within {} polls",
        transaction_id,
        max_polls
    );
}
