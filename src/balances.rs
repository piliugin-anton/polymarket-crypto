//! On-chain balance panel: USDC.e cash + claimable CTF payout (standard markets).
//!
//! * **Cash** — `USDC.e.balanceOf(funder)` (Polymarket collateral token on Polygon).
//! * **Claimable** — for each *standard* (non–neg-risk) redeemable market from the Data API index,
//!   read `payoutDenominator`, `payoutNumerators`, and ERC-1155 balances on the CTF and sum
//!   `balance * numerator / denominator`. Neg-risk rows still use Data API `currentValue` sums
//!   (different position-id scheme; see `redeem.rs`).
//!
//! Reads use direct `eth_call` and JSON-RPC **batch** `eth_call` (no Multicall3 contract) so RPC
//! return data is plain ABI-encoded outputs — avoids Alloy `aggregate3` return decode issues on
//! some hosted Polygon endpoints.
//!
//! Polygon JSON-RPC uses a **direct** HTTP client (no `POLYMARKET_PROXY`); Data API keeps using the
//! proxy-aware client from [`crate::net`].

use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

use alloy_primitives::{address, keccak256, Address, B256, U256};
use alloy_sol_types::{sol, SolCall};
use anyhow::{Context, Result};
use reqwest::Client;
use serde_json::json;

use crate::data_api::{self, DataPosition};
use crate::redeem::parse_condition_id;

const USDC_E: Address = address!("0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174");
const CTF: Address = address!("0x4D97DCd97eC945f40cF65F87097ACe5EA0476045");

/// Max `eth_call` sub-requests per JSON-RPC batch (must stay within provider limits).
const MAX_BATCH_ETH_CALLS: usize = 80;

sol! {
    interface IERC20 {
        function balanceOf(address account) external view returns (uint256);
    }
}

sol! {
    interface ICTF {
        function payoutDenominator(bytes32 conditionId) external view returns (uint256);
        function payoutNumerators(bytes32 conditionId, uint256 index) external view returns (uint256);
        function balanceOf(address account, uint256 id) external view returns (uint256);
    }
}

/// HTTP client for Polygon RPC only — does not use `POLYMARKET_PROXY`.
pub fn polygon_rpc_http_client() -> Result<Client> {
    Client::builder()
        .user_agent("polymarket-btc5m/0.1")
        .timeout(Duration::from_secs(25))
        .build()
        .context("build direct Polygon RPC HTTP client")
}

fn parent_collection_id() -> B256 {
    B256::ZERO
}

/// GCTF `getCollectionId` / `getPositionId` (same as Polymarket CTF layout for binary markets).
fn collection_id(condition_id: B256, index_set: u64) -> B256 {
    let mut buf = Vec::with_capacity(96);
    buf.extend_from_slice(parent_collection_id().as_slice());
    buf.extend_from_slice(condition_id.as_slice());
    buf.extend_from_slice(&U256::from(index_set).to_be_bytes::<32>());
    keccak256(buf)
}

fn position_id(collateral: Address, collection_id: B256) -> U256 {
    let mut buf = Vec::with_capacity(52);
    buf.extend_from_slice(collateral.as_slice());
    buf.extend_from_slice(collection_id.as_slice());
    let h = keccak256(buf);
    U256::from_be_slice(h.as_slice())
}

fn u256_to_usdc_f64(v: U256) -> f64 {
    v.to_string().parse::<f64>().unwrap_or(0.0) / 1_000_000.0
}

fn decode_uint256_return(data: &[u8]) -> Result<U256> {
    if data.len() < 32 {
        anyhow::bail!("returnData too short: {}", data.len());
    }
    Ok(U256::from_be_slice(&data[data.len() - 32..]))
}

async fn rpc_eth_call(
    http: &Client,
    rpc_url: &str,
    to: Address,
    calldata: &[u8],
) -> Result<Vec<u8>> {
    let data_hex = format!(
        "0x{}",
        calldata.iter().map(|b| format!("{b:02x}")).collect::<String>()
    );
    let body = json!({
        "jsonrpc": "2.0",
        "method": "eth_call",
        "params": [{"to": format!("{to:#x}"), "data": data_hex}, "latest"],
        "id": 1
    });
    let resp = http
        .post(rpc_url)
        .json(&body)
        .send()
        .await
        .context("RPC eth_call")?;
    let v: serde_json::Value = resp.json().await.context("RPC JSON")?;
    if let Some(err) = v.get("error") {
        anyhow::bail!("RPC error: {err}");
    }
    let s = v
        .get("result")
        .and_then(|x| x.as_str())
        .context("RPC missing result")?;
    hex::decode(s.trim_start_matches("0x")).context("decode eth_call hex")
}

/// One HTTP round-trip: multiple `eth_call` with ids `0..n-1`, results in order.
async fn rpc_eth_call_batch(
    http: &Client,
    rpc_url: &str,
    items: &[(Address, Vec<u8>)],
) -> Result<Vec<Vec<u8>>> {
    if items.is_empty() {
        return Ok(vec![]);
    }
    let batch: Vec<serde_json::Value> = items
        .iter()
        .enumerate()
        .map(|(i, (to, data))| {
            let data_hex =
                format!("0x{}", data.iter().map(|b| format!("{b:02x}")).collect::<String>());
            json!({
                "jsonrpc": "2.0",
                "method": "eth_call",
                "params": [{"to": format!("{to:#x}"), "data": data_hex}, "latest"],
                "id": i,
            })
        })
        .collect();
    let resp = http
        .post(rpc_url)
        .json(&batch)
        .send()
        .await
        .context("RPC eth_call batch")?;
    let arr: Vec<serde_json::Value> = resp.json().await.context("RPC batch JSON")?;
    let mut by_id: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
    for obj in arr {
        let id = obj
            .get("id")
            .and_then(|x| x.as_u64())
            .context("batch response id")?;
        if let Some(err) = obj.get("error") {
            anyhow::bail!("eth_call batch item {id} error: {err}");
        }
        let s = obj
            .get("result")
            .and_then(|x| x.as_str())
            .with_context(|| format!("batch item {id} missing result"))?;
        let bytes = hex::decode(s.trim_start_matches("0x"))
            .with_context(|| format!("batch item {id} hex"))?;
        by_id.insert(id, bytes);
    }
    if by_id.len() != items.len() {
        anyhow::bail!(
            "batch response count {} != requests {}",
            by_id.len(),
            items.len()
        );
    }
    let mut out = Vec::with_capacity(items.len());
    for i in 0..items.len() {
        let b = by_id
            .remove(&(i as u64))
            .ok_or_else(|| anyhow::anyhow!("missing batch result id {i}"))?;
        out.push(b);
    }
    Ok(out)
}

/// Sum claimable USDC (6 decimals) for standard CTF conditions via batched `eth_call`.
async fn claimable_standard_usdc(
    http: &Client,
    rpc_url: &str,
    funder: Address,
    conditions: &[B256],
) -> Result<f64> {
    if conditions.is_empty() {
        return Ok(0.0);
    }

    let mut total = U256::ZERO;
    let per_cond = 5usize;
    let chunk_conds = (MAX_BATCH_ETH_CALLS / per_cond).max(1);

    for chunk in conditions.chunks(chunk_conds) {
        let mut batch_items: Vec<(Address, Vec<u8>)> = Vec::with_capacity(chunk.len() * per_cond);

        for cond in chunk {
            let col1 = collection_id(*cond, 1);
            let col2 = collection_id(*cond, 2);
            let pos1 = position_id(USDC_E, col1);
            let pos2 = position_id(USDC_E, col2);

            batch_items.push((
                CTF,
                ICTF::payoutDenominatorCall { conditionId: *cond }.abi_encode(),
            ));
            // CTF `payoutNumerators` uses **outcome slot index** 0..n-1 (binary → 0 and 1). Index *sets*
            // 1 and 2 below are only for `getCollectionId` / ERC-1155 `positionId`.
            batch_items.push((
                CTF,
                ICTF::payoutNumeratorsCall {
                    conditionId: *cond,
                    index: U256::from(0u64),
                }
                .abi_encode(),
            ));
            batch_items.push((
                CTF,
                ICTF::payoutNumeratorsCall {
                    conditionId: *cond,
                    index: U256::from(1u64),
                }
                .abi_encode(),
            ));
            batch_items.push((
                CTF,
                ICTF::balanceOfCall {
                    account: funder,
                    id: pos1,
                }
                .abi_encode(),
            ));
            batch_items.push((
                CTF,
                ICTF::balanceOfCall {
                    account: funder,
                    id: pos2,
                }
                .abi_encode(),
            ));
        }

        let raws = rpc_eth_call_batch(http, rpc_url, &batch_items).await?;
        let mut i = 0usize;
        for _cond in chunk {
            if i + per_cond > raws.len() {
                break;
            }
            let denom = decode_uint256_return(&raws[i]).unwrap_or(U256::ZERO);
            let n1 = decode_uint256_return(&raws[i + 1]).unwrap_or(U256::ZERO);
            let n2 = decode_uint256_return(&raws[i + 2]).unwrap_or(U256::ZERO);
            let b1 = decode_uint256_return(&raws[i + 3]).unwrap_or(U256::ZERO);
            let b2 = decode_uint256_return(&raws[i + 4]).unwrap_or(U256::ZERO);
            i += per_cond;

            if denom.is_zero() {
                continue;
            }
            total += b1 * n1 / denom;
            total += b2 * n2 / denom;
        }
    }

    Ok(u256_to_usdc_f64(total))
}

async fn fetch_onchain_cash_and_claim_std(
    http_rpc: &Client,
    rpc_url: &str,
    funder: Address,
    conditions: &[B256],
) -> Result<(f64, f64)> {
    let cash = usdc_e_cash_f64(http_rpc, rpc_url, funder)
        .await
        .context("USDC.e balance eth_call")?;
    let claim_std = claimable_standard_usdc(http_rpc, rpc_url, funder, conditions).await?;
    Ok((cash, claim_std))
}

async fn usdc_e_cash_f64(http: &Client, rpc_url: &str, funder: Address) -> Result<f64> {
    let calldata = IERC20::balanceOfCall { account: funder }.abi_encode();
    let raw = rpc_eth_call(http, rpc_url, USDC_E, &calldata).await?;
    let v = decode_uint256_return(&raw)?;
    Ok(u256_to_usdc_f64(v))
}

fn sum_neg_risk_claimable_usdc(rows: &[DataPosition]) -> f64 {
    rows.iter()
        .filter(|p| p.redeemable && p.negative_risk)
        .map(|p| p.current_value)
        .filter(|v| v.is_finite())
        .sum()
}

/// Cash (USDC.e) + claimable: standard CTF via `eth_call` / batch; neg-risk redeemable rows from Data API values.
pub async fn fetch_balance_panel_usdc(
    http_data_api: &Client,
    http_rpc: &Client,
    rpc_url: &str,
    funder: Address,
) -> Result<(f64, f64)> {
    let redeemable_rows = match data_api::fetch_redeemable_positions(http_data_api, funder).await {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!(error = %e, "data-api redeemable positions (for index + neg-risk) failed");
            Vec::new()
        }
    };

    let mut std_ids: HashSet<B256> = HashSet::new();
    for p in &redeemable_rows {
        if !p.redeemable || p.negative_risk {
            continue;
        }
        if let Ok(cid) = parse_condition_id(&p.condition_id) {
            std_ids.insert(cid);
        }
    }
    let mut conditions: Vec<B256> = std_ids.into_iter().collect();
    conditions.sort_by(|a, b| a.as_slice().cmp(b.as_slice()));

    let claim_neg = sum_neg_risk_claimable_usdc(&redeemable_rows);

    let (cash, claim_std) =
        fetch_onchain_cash_and_claim_std(http_rpc, rpc_url, funder, &conditions).await?;
    let total_claim = claim_std + claim_neg;
    Ok((cash, total_claim))
}
