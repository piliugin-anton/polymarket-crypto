//! Configuration: endpoints, env vars, chain constants.
//!
//! All trading-sensitive values come from environment variables. See `.env.example`.

use anyhow::{bail, Context, Result};
use alloy_primitives::{Address, U256};
use std::str::FromStr;

// ── Polymarket endpoints ────────────────────────────────────────────
pub const CLOB_HOST: &str   = "https://clob.polymarket.com";
pub const GAMMA_HOST: &str  = "https://gamma-api.polymarket.com";
pub const CLOB_WS_URL: &str = "wss://ws-subscriptions-clob.polymarket.com/ws/market";
pub const RTDS_WS_URL: &str = "wss://ws-live-data.polymarket.com";

// ── Polygon ─────────────────────────────────────────────────────────
pub const POLYGON_CHAIN_ID: u64 = 137;

// CTF Exchange + NegRisk Exchange (verifyingContract for EIP-712 domain)
// Source: https://docs.polymarket.com
pub const CTF_EXCHANGE: &str         = "0x4bFb41d5B3570DeFd03C39a9A4D8dE6Bd8B8982E";
pub const NEG_RISK_CTF_EXCHANGE: &str = "0xC5d563A36AE78145C45a50134d48A1215220f80a";

/// Signature type (see CTF Exchange `OrderStructs.sol`).
///
/// * 0 = `EOA` — you hold a plain wallet and trade directly (rare on Polymarket).
/// * 1 = `POLY_PROXY` — legacy magic/email-based proxy wallet.
/// * 2 = `POLY_GNOSIS_SAFE` — the current default: your EOA owns a Gnosis Safe that
///       holds the USDC. `funder` = safe address, `signer` = EOA address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureType {
    Eoa           = 0,
    PolyProxy     = 1,
    PolyGnosisSafe = 2,
}

/// Runtime configuration — resolved once at startup.
#[derive(Debug, Clone)]
pub struct Config {
    /// Private key (hex, 0x-prefixed) of the EOA that signs orders.
    pub private_key: String,
    /// Address that actually funds the trades. For Safe proxy: the Safe address;
    /// for EOA: same as the EOA.
    pub funder: Address,
    /// EOA address derived from `private_key` — filled by `signer.address()`.
    pub signer_address: Address,
    pub sig_type: SignatureType,
    /// Default order size in USDC (human units, e.g. 10.0).
    pub default_size_usdc: f64,
    /// Max fill slippage for market orders, in basis points.
    pub market_slippage_bps: u32,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        // .env is optional — ignore failure
        let _ = dotenvy::dotenv();

        let private_key = std::env::var("POLYMARKET_PK")
            .context("POLYMARKET_PK env var (0x-prefixed hex private key) is required")?;

        let funder_str = std::env::var("POLYMARKET_FUNDER")
            .context("POLYMARKET_FUNDER env var (funder/safe address) is required")?;
        let funder = Address::from_str(&funder_str)
            .context("POLYMARKET_FUNDER is not a valid 0x-prefixed address")?;

        let sig_type = match std::env::var("POLYMARKET_SIG_TYPE")
            .unwrap_or_else(|_| "2".to_string())
            .as_str()
        {
            "0" => SignatureType::Eoa,
            "1" => SignatureType::PolyProxy,
            "2" => SignatureType::PolyGnosisSafe,
            other => bail!("POLYMARKET_SIG_TYPE must be 0, 1, or 2 (got {other})"),
        };

        let default_size_usdc = std::env::var("DEFAULT_SIZE_USDC")
            .ok()
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(5.0);

        let market_slippage_bps = std::env::var("MARKET_SLIPPAGE_BPS")
            .ok()
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(200); // 2% default slippage tolerance on market orders

        // Derive signer address using alloy
        use alloy_signer::Signer as _;
        let signer: alloy_signer_local::PrivateKeySigner = private_key.parse()
            .context("Could not parse POLYMARKET_PK as a private key")?;
        let signer_address = signer.address();

        Ok(Self {
            private_key,
            funder,
            signer_address,
            sig_type,
            default_size_usdc,
            market_slippage_bps,
        })
    }
}

/// Helper: 1 USDC = 1_000_000 base units (USDC has 6 decimals on Polygon).
pub fn usdc_to_base(amount: f64) -> U256 {
    // Multiply in integer space to avoid f64 precision issues.
    let micros = (amount * 1_000_000.0).round() as u128;
    U256::from(micros)
}

/// Convert CTF token amount (6-decimal) to f64 shares.
pub fn base_to_shares(base: U256) -> f64 {
    // Shares are expressed with the same 1e6 scale as USDC on Polymarket.
    let v: u128 = base.try_into().unwrap_or(0);
    (v as f64) / 1_000_000.0
}
