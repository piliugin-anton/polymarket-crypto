//! CTF redemption via Polymarket Relayer (gasless Safe `execTransaction`).
//!
//! After [CLOB V2 / pUSD](https://docs.polymarket.com/v2-migration), resolved shares live under
//! [`ctf-exchange-v2`](https://github.com/Polymarket/ctf-exchange-v2) **collateral adapters**:
//! [`CtfCollateralAdapter`](https://docs.polymarket.com/resources/contracts) and
//! [`NegRiskCtfCollateralAdapter`](https://docs.polymarket.com/resources/contracts) expose the same
//! `redeemPositions(address,bytes32,bytes32,uint256[])` entrypoint (first args unused). They pull CTF
//! ERC1155, call CTF internally with **USDC.e** as `collateralToken`, then wrap proceeds to **pUSD**
//! (PMCT) for `msg.sender`. Calling **CTF** or the **legacy NegRisk adapter** directly from the Safe
//! does not match those position IDs / unwrap paths and typically fails on-chain.
//!
//! Flow matches [`@polymarket/builder-relayer-client`](https://github.com/Polymarket/builder-relayer-client)
//! (`buildSafeTransactionRequest`): EIP-712 `SafeTx` hash → sign → `POST /submit`.
//! Docs: <https://docs.polymarket.com/developers/builders/relayer-client>,
//! <https://docs.polymarket.com/api-reference/relayer/submit-a-transaction>.

use alloy_dyn_abi::eip712::TypedData;
use alloy_primitives::{address, b256, keccak256, Address, B256, U256};
use alloy_signer::Signer;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::{sol, SolCall};
use anyhow::{bail, Context, Result};
use reqwest::Client;
use serde::Deserialize;
use serde_json::json;
use tracing::{error, info, warn};

use crate::config::{Config, SignatureType};
use crate::data_api::DataPosition;

const RELAYER_HOST: &str = "https://relayer-v2.polymarket.com";
/// CtfCollateralAdapter — standard markets redeem entrypoint. [Contracts / Collateral](https://docs.polymarket.com/resources/contracts)
const CTF_COLLATERAL_ADAPTER: Address = address!("0xADa100874d00e3331D00F2007a9c336a65009718");
/// NegRiskCtfCollateralAdapter — neg-risk redeem entrypoint. [Contracts / Collateral](https://docs.polymarket.com/resources/contracts)
const NEG_RISK_CTF_COLLATERAL_ADAPTER: Address =
    address!("0xAdA200001000ef00D07553cEE7006808F895c6F1");
/// Gnosis Safe Factory. [Contracts / Wallet factory](https://docs.polymarket.com/resources/contracts#wallet-factory-contracts)
const SAFE_FACTORY: Address = address!("0xaacFeEa03eb1561C4e67d661e40682Bd20E3541b");
const SAFE_INIT_CODE_HASH: B256 =
    b256!("0x2bce2127ff07fb632d16c8347c4ebf501f4841168bed00d9e6ef715ddb6fcecf");
/// Gnosis `MultiSend` — not listed on Contracts; matches `@polymarket/builder-relayer-client` `getContractConfig(137)`.
const SAFE_MULTISEND: Address = address!("0xA238CBeb142c10Ef7Ad8442C6D1f9E89e07e7761");

sol! {
    /// Same selector/ABI as CTF `redeemPositions`; adapter ignores `collateralToken`, `parentCollectionId`, and `indexSets`.
    contract CtfCollateralAdapter {
        function redeemPositions(
            address collateralToken,
            bytes32 parentCollectionId,
            bytes32 conditionId,
            uint256[] indexSets
        ) external;
    }
}

sol! {
    contract MultiSend {
        function multiSend(bytes transactions) external;
    }
}

#[derive(Deserialize)]
struct NonceResponse {
    nonce: String,
}

#[derive(Deserialize)]
struct DeployedResponse {
    deployed: bool,
}

#[derive(Deserialize)]
struct SubmitResponse {
    #[serde(rename = "transactionID")]
    transaction_id: String,
    #[serde(default)]
    #[allow(dead_code)]
    transaction_hash: String,
    state: String,
}

/// Polymarket CREATE2 Safe for browser-wallet users (`derive_safe_wallet` in `polymarket_client_sdk_v2`).
fn derive_polymarket_safe(eoa: Address) -> Address {
    let mut padded = [0_u8; 32];
    padded[12..].copy_from_slice(eoa.as_slice());
    let salt = keccak256(padded);
    SAFE_FACTORY.create2(salt, SAFE_INIT_CODE_HASH)
}

fn encode_v2_adapter_redeem(condition_id: B256) -> Vec<u8> {
    CtfCollateralAdapter::redeemPositionsCall {
        collateralToken: Address::ZERO,
        parentCollectionId: B256::ZERO,
        conditionId: condition_id,
        indexSets: vec![],
    }
    .abi_encode()
}

/// One inner call for Gnosis `MultiSend.multiSend` (`abi.encodePacked` per sub-tx).
fn gnosis_multisend_pack_inner_call(to: Address, value: U256, data: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(1 + 20 + 32 + 32 + data.len());
    v.push(0u8); // OperationType.Call (delegatecall only on the outer Safe tx)
    v.extend_from_slice(to.as_slice());
    v.extend_from_slice(&value.to_be_bytes::<32>());
    let len = U256::from(data.len());
    v.extend_from_slice(&len.to_be_bytes::<32>());
    v.extend_from_slice(data);
    v
}

fn encode_safe_multisend_calldata_from_packed(packed: Vec<u8>) -> Result<Vec<u8>> {
    if packed.is_empty() {
        bail!("multiSend: empty batch");
    }
    Ok(MultiSend::multiSendCall {
        transactions: packed.into(),
    }
    .abi_encode())
}

/// Pack ECDSA signature for Polymarket Safe relayer (see `builder-relayer-client` `splitAndPackSig`).
fn pack_safe_rel_signature(mut sig: [u8; 65]) -> Result<String> {
    let mut v = u16::from(sig[64]);
    match v {
        0 | 1 => v += 31,
        27 | 28 => v += 4,
        _ => bail!("unexpected signature v byte: {}", sig[64]),
    }
    sig[64] = v as u8;
    let r = U256::from_be_slice(&sig[..32]);
    let s = U256::from_be_slice(&sig[32..64]);
    let vb = sig[64] as u64;
    let mut packed = Vec::with_capacity(65);
    packed.extend_from_slice(&r.to_be_bytes::<32>());
    packed.extend_from_slice(&s.to_be_bytes::<32>());
    packed.push(vb as u8);
    Ok(format!(
        "0x{}",
        packed.iter().map(|b| format!("{b:02x}")).collect::<String>()
    ))
}

fn safe_typed_data_digest(
    chain_id: u64,
    safe: Address,
    to: Address,
    data: &[u8],
    operation: u8,
    nonce: &str,
) -> Result<B256> {
    let data_hex = format!(
        "0x{}",
        data.iter().map(|b| format!("{b:02x}")).collect::<String>()
    );
    let json = json!({
        "types": {
            "EIP712Domain": [
                {"name": "chainId", "type": "uint256"},
                {"name": "verifyingContract", "type": "address"}
            ],
            "SafeTx": [
                {"name": "to", "type": "address"},
                {"name": "value", "type": "uint256"},
                {"name": "data", "type": "bytes"},
                {"name": "operation", "type": "uint8"},
                {"name": "safeTxGas", "type": "uint256"},
                {"name": "baseGas", "type": "uint256"},
                {"name": "gasPrice", "type": "uint256"},
                {"name": "gasToken", "type": "address"},
                {"name": "refundReceiver", "type": "address"},
                {"name": "nonce", "type": "uint256"}
            ]
        },
        "primaryType": "SafeTx",
        "domain": {
            "chainId": chain_id,
            "verifyingContract": format!("{safe:#x}")
        },
        "message": {
            "to": format!("{to:#x}"),
            "value": "0",
            "data": data_hex,
            "operation": operation,
            "safeTxGas": "0",
            "baseGas": "0",
            "gasPrice": "0",
            "gasToken": "0x0000000000000000000000000000000000000000",
            "refundReceiver": "0x0000000000000000000000000000000000000000",
            "nonce": nonce
        }
    });
    let td: TypedData = serde_json::from_value(json).context("EIP-712 JSON for SafeTx")?;
    td.eip712_signing_hash()
        .map_err(|e| anyhow::anyhow!("EIP-712 hash: {e}"))
}

async fn relayer_get_nonce(http: &Client, signer: Address) -> Result<String> {
    let url = format!("{RELAYER_HOST}/nonce?address={signer:#x}&type=SAFE");
    let resp = http.get(&url).send().await.context("relayer GET /nonce")?;
    let status = resp.status();
    let txt = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("relayer /nonce failed: {status} — {}", txt.trim());
    }
    let n: NonceResponse = serde_json::from_str(&txt)
        .with_context(|| format!("decode /nonce: {}", txt.trim()))?;
    Ok(n.nonce)
}

async fn relayer_deployed(http: &Client, proxy_wallet: Address) -> Result<bool> {
    let url = format!("{RELAYER_HOST}/deployed?address={proxy_wallet:#x}");
    let resp = http
        .get(&url)
        .send()
        .await
        .context("relayer GET /deployed")?;
    let status = resp.status();
    let txt = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("relayer /deployed failed: {status} — {}", txt.trim());
    }
    let d: DeployedResponse = serde_json::from_str(&txt)
        .with_context(|| format!("decode /deployed: {}", txt.trim()))?;
    Ok(d.deployed)
}

async fn relayer_submit(
    http: &Client,
    relayer_key: &str,
    relayer_key_addr: Address,
    body: serde_json::Value,
) -> Result<SubmitResponse> {
    info!(
        proxy_wallet = %body.get("proxyWallet").and_then(|v| v.as_str()).unwrap_or("?"),
        to = %body.get("to").and_then(|v| v.as_str()).unwrap_or("?"),
        nonce = %body.get("nonce").and_then(|v| v.as_str()).unwrap_or("?"),
        "relayer POST /submit: sending transaction"
    );
    let resp = http
        .post(format!("{RELAYER_HOST}/submit"))
        .header("RELAYER_API_KEY", relayer_key)
        .header("RELAYER_API_KEY_ADDRESS", format!("{relayer_key_addr:#x}"))
        .json(&body)
        .send()
        .await
        .with_context(|| {
            error!("relayer POST /submit: transport error before HTTP status");
            "relayer POST /submit"
        })?;
    let status = resp.status();
    let txt = resp.text().await.unwrap_or_default();
    let body_trim = txt.trim();
    if !status.is_success() {
        error!(
            status = %status,
            response_body = %body_trim,
            "relayer POST /submit: HTTP error"
        );
        bail!("relayer /submit failed: {status} — {}", body_trim);
    }
    match serde_json::from_str::<SubmitResponse>(body_trim) {
        Ok(out) => {
            info!(
                transaction_id = %out.transaction_id,
                state = %out.state,
                response_body = %body_trim,
                "relayer POST /submit: success"
            );
            Ok(out)
        }
        Err(e) => {
            error!(
                error = %e,
                response_body = %body_trim,
                "relayer POST /submit: JSON decode error (HTTP 2xx)"
            );
            Err(e).with_context(|| format!("decode /submit: {body_trim}"))
        }
    }
}

pub(crate) fn parse_condition_id(s: &str) -> Result<B256> {
    let t = s.trim();
    let h = t.strip_prefix("0x").unwrap_or(t);
    let b = hex::decode(h).context("conditionId hex")?;
    if b.len() != 32 {
        bail!("conditionId must be 32 bytes, got {}", b.len());
    }
    Ok(B256::from_slice(&b))
}

pub(crate) fn parse_token_id_u256(s: &str) -> Result<U256> {
    let t = s.trim();
    if let Some(h) = t.strip_prefix("0x") {
        let b = hex::decode(h).context("asset id hex")?;
        return Ok(U256::from_be_slice(&b));
    }
    U256::from_str_radix(t, 10).context("asset id decimal")
}

/// Redeem all redeemable positions returned by Data API in **one** relayer submission when possible:
/// multiple adapter `redeemPositions` calls are packed with Gnosis `MultiSend` + Safe `DelegateCall`, matching
/// [`aggregateTransaction`](https://github.com/Polymarket/builder-relayer-client/blob/main/src/builder/safe.ts).
pub async fn redeem_resolved_positions(
    cfg: &Config,
    http: &Client,
    positions: &[DataPosition],
) -> Result<String> {
    if cfg.sig_type != SignatureType::PolyGnosisSafe {
        bail!(
            "CTF redeem via relayer supports POLYMARKET_SIG_TYPE=2 (Gnosis Safe) only. \
             For EOA/proxy wallets use polymarket.com Portfolio or the official CLI."
        );
    }
    let rel_key = cfg
        .relayer_api_key
        .as_deref()
        .filter(|s| !s.is_empty())
        .context(
            "set POLYMARKET_RELAYER_API_KEY (+ POLYMARKET_RELAYER_API_KEY_ADDRESS) — create at \
             polymarket.com → Settings → API (Relayer)",
        )?;
    let rel_addr = cfg
        .relayer_api_key_address
        .context("POLYMARKET_RELAYER_API_KEY_ADDRESS")?;

    let signer: PrivateKeySigner = cfg
        .private_key
        .parse()
        .context("parse POLYMARKET_PK")?;
    let derived_safe = derive_polymarket_safe(cfg.signer_address);
    if derived_safe != cfg.funder {
        bail!(
            "POLYMARKET_FUNDER ({:#x}) != derived Safe ({:#x}) for this EOA — check env",
            cfg.funder,
            derived_safe
        );
    }
    if !relayer_deployed(http, cfg.funder).await? {
        bail!("Safe not deployed on-chain yet — use polymarket.com once before redeeming");
    }

    let mut redeemable: Vec<&DataPosition> = positions.iter().filter(|p| p.redeemable).collect();
    redeemable.sort_by(|a, b| a.condition_id.cmp(&b.condition_id));
    if redeemable.is_empty() {
        bail!("no redeemable positions from Data API");
    }

    let mut seen = std::collections::HashSet::new();
    let mut ops: Vec<(String, Address, Vec<u8>)> = Vec::new();

    for p in redeemable {
        if !seen.insert(p.condition_id.as_str()) {
            continue;
        }
        let short = p.condition_id.chars().take(10).collect::<String>();
        let condition = match parse_condition_id(&p.condition_id) {
            Ok(c) => c,
            Err(e) => {
                warn!(cond = %p.condition_id, error = %e, "CTF redeem: skip (bad conditionId)");
                continue;
            }
        };
        let adapter = if p.negative_risk {
            NEG_RISK_CTF_COLLATERAL_ADAPTER
        } else {
            CTF_COLLATERAL_ADAPTER
        };
        ops.push((short, adapter, encode_v2_adapter_redeem(condition)));
    }

    if ops.is_empty() {
        bail!("CTF redeem: nothing to redeem (all rows skipped or no redeemable markets)");
    }

    let (relay_to, calldata, safe_operation) = if ops.len() == 1 {
        let (_, t, d) = ops.pop().expect("len==1");
        (t, d, 0u8)
    } else {
        let mut packed = Vec::new();
        for (_, t, d) in &ops {
            packed.extend(gnosis_multisend_pack_inner_call(*t, U256::ZERO, d));
        }
        let data = encode_safe_multisend_calldata_from_packed(packed)?;
        (SAFE_MULTISEND, data, 1u8)
    };

    let nonce = relayer_get_nonce(http, cfg.signer_address).await?;
    let digest = safe_typed_data_digest(
        crate::config::POLYGON_CHAIN_ID,
        cfg.funder,
        relay_to,
        &calldata,
        safe_operation,
        &nonce,
    )?;
    // Polymarket `buildSafeTransactionRequest` signs the EIP-712 struct hash with
    // `signMessage(hash)` (viem/ethers) → EIP-191 `personal_sign` over the 32-byte digest,
    // **not** raw ECDSA on the digest. See `builder-relayer-client` `createSafeSignature`.
    let sig = signer
        .sign_message(digest.as_slice())
        .await
        .context("sign SafeTx digest (EIP-191 over EIP-712 hash, relayer-compatible)")?;
    let sig_bytes: [u8; 65] = sig.as_bytes();
    let packed = pack_safe_rel_signature(sig_bytes)?;

    let req = json!({
        "from": format!("{:#x}", cfg.signer_address),
        "to": format!("{relay_to:#x}"),
        "proxyWallet": format!("{:#x}", cfg.funder),
        "data": format!(
            "0x{}",
            calldata.iter().map(|b| format!("{b:02x}")).collect::<String>()
        ),
        "nonce": nonce,
        "signature": packed,
        "signatureParams": {
            "gasPrice": "0",
            "operation": format!("{safe_operation}"),
            "safeTxnGas": "0",
            "baseGas": "0",
            "gasToken": "0x0000000000000000000000000000000000000000",
            "refundReceiver": "0x0000000000000000000000000000000000000000"
        },
        "type": "SAFE",
        "metadata": "polymarket-crypto redeem-all"
    });

    let out = relayer_submit(http, rel_key, rel_addr, req).await?;
    let markets = ops.len();
    let ids = ops
        .iter()
        .map(|(s, _, _)| s.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    Ok(format!(
        "{markets} market(s){} → relayer {} ({}) [{ids}]",
        if safe_operation == 1 { " (MultiSend batch)" } else { "" },
        out.transaction_id,
        out.state
    ))
}
