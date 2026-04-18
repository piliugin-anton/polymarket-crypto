//! Trading layer — EIP-712 order construction + CLOB REST submission.
//!
//! We do the signing in pure Rust using `alloy` + `alloy-sol-types`. The
//! order struct is lifted verbatim from Polymarket's `ctf-exchange`:
//!
//! ```solidity
//! struct Order {
//!     uint256 salt;
//!     address maker;
//!     address signer;
//!     address taker;        // 0x0 = public order
//!     uint256 tokenId;
//!     uint256 makerAmount;
//!     uint256 takerAmount;
//!     uint256 expiration;   // 0 = GTC
//!     uint256 nonce;
//!     uint256 feeRateBps;
//!     uint8   side;         // 0=BUY, 1=SELL
//!     uint8   signatureType;
//! }
//! ```
//!
//! L1 (EIP-712 wallet sig) is used ONCE to derive API credentials. After that
//! every order-posting request is authed with L2 (HMAC-SHA256 over
//! `timestamp + method + path + body` using the base64-decoded `secret`).

use anyhow::{anyhow, Context, Result};
use alloy_primitives::{Address, U256, B256, hex};
use alloy_signer::Signer;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::{sol, SolStruct, Eip712Domain, eip712_domain};
use base64::Engine as _;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::str::FromStr;

use crate::config::{
    Config, SignatureType, CTF_EXCHANGE, CLOB_HOST, NEG_RISK_CTF_EXCHANGE, POLYGON_CHAIN_ID,
    usdc_to_base,
};

// ── EIP-712 Order struct (derived via alloy-sol-types) ──────────────
sol! {
    #[derive(Debug)]
    struct Order {
        uint256 salt;
        address maker;
        address signer;
        address taker;
        uint256 tokenId;
        uint256 makerAmount;
        uint256 takerAmount;
        uint256 expiration;
        uint256 nonce;
        uint256 feeRateBps;
        uint8   side;
        uint8   signatureType;
    }
}

fn domain(verifying_contract: Address) -> Eip712Domain {
    eip712_domain! {
        name:              "Polymarket CTF Exchange",
        version:           "1",
        chain_id:          POLYGON_CHAIN_ID,
        verifying_contract: verifying_contract,
    }
}

// ── Public types ────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side { Buy, Sell }

impl Side {
    fn as_u8(self) -> u8 { match self { Side::Buy => 0, Side::Sell => 1 } }
    fn as_str(self) -> &'static str { match self { Side::Buy => "BUY", Side::Sell => "SELL" } }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderType { Gtc, Fok, Fak, Gtd }

impl OrderType {
    fn as_str(self) -> &'static str { match self {
        OrderType::Gtc => "GTC", OrderType::Fok => "FOK",
        OrderType::Fak => "FAK", OrderType::Gtd => "GTD",
    } }
}

#[derive(Debug, Clone)]
pub struct OrderArgs {
    pub token_id: String,
    pub side:     Side,
    pub price:    f64,     // 0.01..=0.99
    pub size:     f64,     // in shares
    pub neg_risk: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiCreds {
    #[serde(rename = "apiKey")] pub api_key: String,
    pub secret:     String, // base64
    pub passphrase: String,
}

#[derive(Debug, Deserialize)]
pub struct PostOrderResponse {
    #[serde(default)] pub success:  bool,
    #[serde(default)] #[serde(rename = "orderID")]
    pub order_id: Option<String>,
    #[serde(default)] pub status:   Option<String>,
    #[serde(default, rename = "errorMsg")]
    pub error:    Option<String>,
}

// ── Client ──────────────────────────────────────────────────────────

pub struct TradingClient {
    http:    reqwest::Client,
    signer:  PrivateKeySigner,
    creds:   Option<ApiCreds>,
    config:  Config,
}

impl TradingClient {
    pub fn new(config: Config) -> Result<Self> {
        let signer: PrivateKeySigner = config.private_key.parse()
            .context("parsing private key")?;
        Ok(Self {
            http: reqwest::Client::builder()
                .user_agent("btc5m-bot/0.1")
                .timeout(std::time::Duration::from_secs(10))
                .build()?,
            signer,
            creds: None,
            config,
        })
    }

    /// Derive (or create) the L2 API credentials by producing an EIP-712
    /// signature over the `ClobAuth` struct. Tries GET /auth/derive-api-key
    /// first and falls back to POST /auth/api-key if no keys exist yet.
    pub async fn ensure_creds(&mut self) -> Result<ApiCreds> {
        if let Some(c) = &self.creds { return Ok(c.clone()); }

        let ts: i64   = chrono::Utc::now().timestamp();
        let nonce: u64 = 0;
        let sig  = self.sign_clob_auth(ts, nonce).await?;
        let addr = format!("{:#x}", self.signer.address());

        let headers = [
            ("POLY_ADDRESS",   addr.clone()),
            ("POLY_SIGNATURE", sig.clone()),
            ("POLY_TIMESTAMP", ts.to_string()),
            ("POLY_NONCE",     nonce.to_string()),
        ];

        // 1) Try derive (GET) — returns existing creds if they exist.
        let mut req = self.http.get(format!("{CLOB_HOST}/auth/derive-api-key"));
        for (k, v) in &headers { req = req.header(*k, v); }
        let resp = req.send().await.context("GET /auth/derive-api-key")?;

        if resp.status().is_success() {
            let creds: ApiCreds = resp.json().await.context("decoding derived creds")?;
            self.creds = Some(creds.clone());
            return Ok(creds);
        }

        // 2) No creds yet — create them.
        let mut req = self.http.post(format!("{CLOB_HOST}/auth/api-key"));
        for (k, v) in &headers { req = req.header(*k, v); }
        let resp = req.send().await.context("POST /auth/api-key")?;
        let status = resp.status();
        let body   = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(anyhow!("could not create API key: {status} — {body}"));
        }
        let creds: ApiCreds = serde_json::from_str(&body).context("decoding created creds")?;
        self.creds = Some(creds.clone());
        Ok(creds)
    }

    /// Sign the `ClobAuth` EIP-712 struct (used to derive/create API keys).
    async fn sign_clob_auth(&self, ts: i64, nonce: u64) -> Result<String> {
        sol! {
            struct ClobAuth {
                address addr;
                string  timestamp;
                uint256 nonce;
                string  message;
            }
        }
        let dom = eip712_domain! {
            name:     "ClobAuthDomain",
            version:  "1",
            chain_id: POLYGON_CHAIN_ID,
        };
        let msg = ClobAuth {
            addr:      self.signer.address(),
            timestamp: ts.to_string(),
            nonce:     U256::from(nonce),
            message:   "This message attests that I control the given wallet".into(),
        };
        let hash = msg.eip712_signing_hash(&dom);
        let sig  = self.signer.sign_hash(&hash).await?;
        Ok(format!("0x{}", hex::encode(sig.as_bytes())))
    }

    /// Build + sign an order and POST it to the CLOB.
    pub async fn place_order(&mut self, args: OrderArgs, order_type: OrderType)
        -> Result<PostOrderResponse>
    {
        let creds = self.ensure_creds().await?;

        // 1. Compute maker/taker amounts from price+size.
        //    BUY:  makerAmount = size * price (USDC),  takerAmount = size (shares)
        //    SELL: makerAmount = size (shares),        takerAmount = size * price (USDC)
        let (maker_amount, taker_amount) = amounts_for(args.side, args.size, args.price);

        // 2. Build the struct.
        let salt  = rand::random::<u64>();
        let nonce = 0u64; // Polymarket CLOB uses 0 unless you're cancelling on-chain by nonce
        let verifying = if args.neg_risk { NEG_RISK_CTF_EXCHANGE } else { CTF_EXCHANGE };
        let verifying_addr = Address::from_str(verifying)?;

        let order = Order {
            salt:          U256::from(salt),
            maker:         self.config.funder,             // funder address
            signer:        self.signer.address(),           // EOA that signs
            taker:         Address::ZERO,                   // public order
            tokenId:       U256::from_str(&args.token_id).context("token_id")?,
            makerAmount:   maker_amount,
            takerAmount:   taker_amount,
            expiration:    U256::ZERO,                      // GTC
            nonce:         U256::from(nonce),
            feeRateBps:    U256::ZERO,                      // fees are currently zero on PM
            side:          args.side.as_u8(),
            signatureType: self.config.sig_type as u8,
        };

        // 3. Sign with EIP-712.
        let dom  = domain(verifying_addr);
        let hash = order.eip712_signing_hash(&dom);
        let sig  = self.signer.sign_hash(&hash).await?;
        let sig_hex = format!("0x{}", hex::encode(sig.as_bytes()));

        // 4. Compute the order hash (used as orderID).
        let order_hash: B256 = hash;

        // 5. POST /order with L2 auth.
        #[derive(Serialize)]
        struct OrderPayload<'a> {
            order: SignedOrder<'a>,
            owner: &'a str,
            #[serde(rename = "orderType")] order_type: &'a str,
        }
        #[derive(Serialize)]
        struct SignedOrder<'a> {
            salt:           String,
            maker:          String,
            signer:         String,
            taker:          String,
            #[serde(rename = "tokenId")] token_id: String,
            #[serde(rename = "makerAmount")] maker_amount: String,
            #[serde(rename = "takerAmount")] taker_amount: String,
            side:           &'a str,
            expiration:     String,
            nonce:          String,
            #[serde(rename = "feeRateBps")] fee_rate_bps: String,
            #[serde(rename = "signatureType")] signature_type: u8,
            signature:      String,
            hash:           String,
        }
        let payload = OrderPayload {
            order: SignedOrder {
                salt:           salt.to_string(),
                maker:          format!("{:#x}", order.maker),
                signer:         format!("{:#x}", order.signer),
                taker:          format!("{:#x}", order.taker),
                token_id:       order.tokenId.to_string(),
                maker_amount:   order.makerAmount.to_string(),
                taker_amount:   order.takerAmount.to_string(),
                side:           args.side.as_str(),
                expiration:     "0".into(),
                nonce:          "0".into(),
                fee_rate_bps:   "0".into(),
                signature_type: self.config.sig_type as u8,
                signature:      sig_hex,
                hash:           format!("0x{}", hex::encode(order_hash)),
            },
            owner:      &creds.api_key,
            order_type: order_type.as_str(),
        };
        let body = serde_json::to_string(&payload)?;

        let ts = chrono::Utc::now().timestamp();
        let path = "/order";
        let l2_sig = l2_hmac(&creds.secret, ts, "POST", path, &body)?;

        let resp = self.http
            .post(format!("{CLOB_HOST}{path}"))
            .header("POLY_ADDRESS",     format!("{:#x}", self.signer.address()))
            .header("POLY_API_KEY",     &creds.api_key)
            .header("POLY_PASSPHRASE",  &creds.passphrase)
            .header("POLY_TIMESTAMP",   ts.to_string())
            .header("POLY_SIGNATURE",   l2_sig)
            .header("Content-Type",     "application/json")
            .body(body)
            .send()
            .await?;

        let status = resp.status();
        let txt    = resp.text().await?;
        if !status.is_success() {
            return Err(anyhow!("CLOB POST /order failed: {} — {}", status, txt));
        }
        serde_json::from_str(&txt).context("decoding order response")
    }

    /// Cancel all open orders for this user.
    pub async fn cancel_all(&mut self) -> Result<()> {
        let creds = self.ensure_creds().await?;
        let ts = chrono::Utc::now().timestamp();
        let path = "/cancel-all";
        let l2_sig = l2_hmac(&creds.secret, ts, "DELETE", path, "")?;
        let resp = self.http
            .delete(format!("{CLOB_HOST}{path}"))
            .header("POLY_ADDRESS",    format!("{:#x}", self.signer.address()))
            .header("POLY_API_KEY",    &creds.api_key)
            .header("POLY_PASSPHRASE", &creds.passphrase)
            .header("POLY_TIMESTAMP",  ts.to_string())
            .header("POLY_SIGNATURE",  l2_sig)
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(anyhow!("cancel-all failed: {}", resp.status()));
        }
        Ok(())
    }
}

/// price×size → (makerAmount, takerAmount) in base units (1e6 decimals).
fn amounts_for(side: Side, size_shares: f64, price: f64) -> (U256, U256) {
    let notional = size_shares * price;
    match side {
        Side::Buy  => (usdc_to_base(notional),    usdc_to_base(size_shares)),
        Side::Sell => (usdc_to_base(size_shares), usdc_to_base(notional)),
    }
}

/// L2 HMAC-SHA256 signature. Matches Polymarket's reference:
/// `base64url( HMAC_SHA256(base64_decode(secret), ts+method+path+body) )`.
fn l2_hmac(secret_b64: &str, ts: i64, method: &str, path: &str, body: &str) -> Result<String> {
    // Polymarket's secrets are base64 url-safe, sometimes without padding.
    let key = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(secret_b64)
        .or_else(|_| base64::engine::general_purpose::STANDARD.decode(secret_b64))
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(secret_b64))
        .context("decoding L2 secret")?;
    let mut mac = <Hmac<Sha256>>::new_from_slice(&key).map_err(|e| anyhow!("hmac key: {e}"))?;
    mac.update(ts.to_string().as_bytes());
    mac.update(method.as_bytes());
    mac.update(path.as_bytes());
    mac.update(body.as_bytes());
    let sig = mac.finalize().into_bytes();
    Ok(base64::engine::general_purpose::URL_SAFE.encode(sig))
}
