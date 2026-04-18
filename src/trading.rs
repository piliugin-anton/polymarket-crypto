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
            http: crate::net::reqwest_client()?,
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
        let auth = self.compute_clob_auth(ts, nonce).await?;

        tracing::debug!(
            addr = %auth.address, ts, nonce,
            digest  = %auth.digest,
            sig     = %auth.signature,
            "L1 auth material computed",
        );

        // 1) Try derive (GET) — returns existing creds if they exist.
        let (get_status, get_body) = self.send_auth(
            reqwest::Method::GET, "/auth/derive-api-key", &auth,
        ).await?;
        tracing::info!(
            status = %get_status,
            body   = %snip(&get_body),
            "GET /auth/derive-api-key",
        );

        if get_status.is_success() {
            let creds: ApiCreds = serde_json::from_str(&get_body)
                .with_context(|| format!("decoding derived creds: {}", snip(&get_body)))?;
            tracing::info!(
                api_key_prefix = &creds.api_key[..std::cmp::min(8, creds.api_key.len())],
                "derived existing CLOB API creds",
            );
            self.creds = Some(creds.clone());
            return Ok(creds);
        }

        // 2) Fallback: create fresh creds.
        let (post_status, post_body) = self.send_auth(
            reqwest::Method::POST, "/auth/api-key", &auth,
        ).await?;
        tracing::info!(
            status = %post_status,
            body   = %snip(&post_body),
            "POST /auth/api-key",
        );

        if post_status.is_success() {
            let creds: ApiCreds = serde_json::from_str(&post_body)
                .with_context(|| format!("decoding created creds: {}", snip(&post_body)))?;
            tracing::info!(
                api_key_prefix = &creds.api_key[..std::cmp::min(8, creds.api_key.len())],
                "created new CLOB API creds",
            );
            self.creds = Some(creds.clone());
            return Ok(creds);
        }

        // Both endpoints rejected — emit the full forensic record at error
        // level so it's visible regardless of RUST_LOG setting.
        tracing::error!(
            get_status   = %get_status,
            get_body     = %get_body,
            post_status  = %post_status,
            post_body    = %post_body,
            address      = %auth.address,
            timestamp    = auth.timestamp,
            digest       = %auth.digest,
            signature    = %auth.signature,
            "CLOB auth rejected by both endpoints",
        );
        Err(anyhow!(
            "CLOB auth failed. GET /auth/derive-api-key → {} \"{}\". POST /auth/api-key → {} \"{}\". Run `btc5m-bot debug-auth` for a full dump.",
            get_status, snip(&get_body), post_status, snip(&post_body),
        ))
    }

    /// Send one of the L1-authed auth endpoints. Returns `(status, body)`.
    async fn send_auth(
        &self,
        method: reqwest::Method,
        path:   &str,
        auth:   &ClobAuthDebug,
    ) -> Result<(reqwest::StatusCode, String)> {
        let url = format!("{CLOB_HOST}{path}");
        let req = self.http.request(method, &url)
            .header("POLY_ADDRESS",   &auth.address)
            .header("POLY_SIGNATURE", &auth.signature)
            .header("POLY_TIMESTAMP", auth.timestamp.to_string())
            .header("POLY_NONCE",     auth.nonce.to_string());
        let resp = req.send().await.with_context(|| format!("{url}"))?;
        let status = resp.status();
        let body   = resp.text().await.unwrap_or_default();
        Ok((status, body))
    }

    /// Full breakdown of the L1 auth material. Used by `ensure_creds` and
    /// by the `debug-auth` diagnostic subcommand.
    async fn compute_clob_auth(&self, ts: i64, nonce: u64) -> Result<ClobAuthDebug> {
        use alloy_primitives::keccak256;

        // Domain separator via alloy's helper (the domain struct has no
        // reserved-keyword issue).
        let dom = eip712_domain! {
            name:     "ClobAuthDomain",
            version:  "1",
            chain_id: POLYGON_CHAIN_ID,
        };
        let domain_sep: B256 = dom.separator();

        // `ClobAuth(address address,string timestamp,uint256 nonce,string message)`
        // — computed by hand because `address` is reserved in the sol! macro.
        let type_hash = keccak256(
            b"ClobAuth(address address,string timestamp,uint256 nonce,string message)"
        );
        let ts_str  = ts.to_string();
        let message = "This message attests that I control the given wallet";

        let mut buf = Vec::with_capacity(32 * 5);
        buf.extend_from_slice(type_hash.as_slice());
        buf.extend_from_slice(&[0u8; 12]);
        buf.extend_from_slice(self.signer.address().as_slice());
        buf.extend_from_slice(keccak256(ts_str.as_bytes()).as_slice());
        let mut nonce_be = [0u8; 32];
        nonce_be[24..].copy_from_slice(&nonce.to_be_bytes());
        buf.extend_from_slice(&nonce_be);
        buf.extend_from_slice(keccak256(message.as_bytes()).as_slice());
        let struct_hash = keccak256(&buf);

        let mut digest_in = [0u8; 2 + 32 + 32];
        digest_in[0] = 0x19;
        digest_in[1] = 0x01;
        digest_in[2..34].copy_from_slice(domain_sep.as_slice());
        digest_in[34..].copy_from_slice(struct_hash.as_slice());
        let digest: B256 = keccak256(&digest_in);

        let sig = self.signer.sign_hash(&digest).await?;

        Ok(ClobAuthDebug {
            address:     format!("{:#x}", self.signer.address()),
            timestamp:   ts,
            nonce,
            type_hash:   hex::encode(type_hash),
            domain_sep:  hex::encode(domain_sep),
            struct_hash: hex::encode(struct_hash),
            digest:      hex::encode(digest),
            signature:   format!("0x{}", hex::encode(sig.as_bytes())),
        })
    }

    /// One-shot diagnostic flow — prints every intermediate to stdout and
    /// attempts both auth endpoints, exiting after. Invoked via
    /// `btc5m-bot debug-auth`.
    pub async fn debug_auth_flow(&mut self) -> Result<()> {
        println!("\n━━━ Polymarket CLOB auth diagnostic ━━━\n");

        let signer_addr = self.signer.address();
        let funder = self.config.funder;
        let sig_type = self.config.sig_type as u8;

        println!("signer (EOA) : {signer_addr:#x}");
        println!("funder       : {funder:#x}");
        println!("sig_type     : {sig_type} ({:?})", self.config.sig_type);
        println!("proxy        : {}", crate::net::proxy_env().as_deref().unwrap_or("<none>"));
        println!("CLOB host    : {CLOB_HOST}\n");

        // Sanity checks for common misconfigurations.
        if signer_addr == funder && sig_type != 0 {
            println!("⚠  POLYMARKET_FUNDER equals your signer address but POLYMARKET_SIG_TYPE={sig_type}.");
            println!("   For a plain EOA wallet set POLYMARKET_SIG_TYPE=0.");
            println!("   For a Safe proxy, POLYMARKET_FUNDER must be the Safe address (not the EOA).\n");
        }
        if signer_addr != funder && sig_type == 0 {
            println!("⚠  POLYMARKET_FUNDER differs from your signer but POLYMARKET_SIG_TYPE=0 (EOA).");
            println!("   If the funder is a Gnosis Safe you own, set POLYMARKET_SIG_TYPE=2.\n");
        }

        let ts = chrono::Utc::now().timestamp();
        let auth = self.compute_clob_auth(ts, 0).await?;

        println!("L1 auth material:");
        println!("  timestamp    {}  ({})", auth.timestamp,
            chrono::DateTime::<chrono::Utc>::from_timestamp(auth.timestamp, 0)
                .map(|d| d.to_rfc3339()).unwrap_or_default());
        println!("  nonce        {}", auth.nonce);
        println!("  type_hash    0x{}", auth.type_hash);
        println!("  domain_sep   0x{}", auth.domain_sep);
        println!("  struct_hash  0x{}", auth.struct_hash);
        println!("  digest       0x{}", auth.digest);
        println!("  signature    {}\n", auth.signature);

        // Request 1: GET derive
        println!("→ GET {CLOB_HOST}/auth/derive-api-key");
        let (s, b) = self.send_auth(reqwest::Method::GET, "/auth/derive-api-key", &auth).await?;
        println!("← {} {}", s.as_u16(), s.canonical_reason().unwrap_or(""));
        println!("  {}\n", b.trim());
        if s.is_success() {
            let creds: ApiCreds = serde_json::from_str(&b)?;
            println!("✓ existing credentials derived — you're set up correctly.");
            println!("  apiKey     {}", creds.api_key);
            return Ok(());
        }

        // Request 2: POST create
        println!("→ POST {CLOB_HOST}/auth/api-key");
        let (s2, b2) = self.send_auth(reqwest::Method::POST, "/auth/api-key", &auth).await?;
        println!("← {} {}", s2.as_u16(), s2.canonical_reason().unwrap_or(""));
        println!("  {}\n", b2.trim());
        if s2.is_success() {
            let creds: ApiCreds = serde_json::from_str(&b2)?;
            println!("✓ new credentials created — subsequent runs will derive these.");
            println!("  apiKey     {}", creds.api_key);
            return Ok(());
        }

        // Neither worked. Print a tailored troubleshooting hint.
        println!("✗ both endpoints failed.  Likely causes:");
        println!();
        match s.as_u16() {
            401 => println!("  401 on derive → L1 signature does not recover to POLY_ADDRESS."),
            _ => {}
        }
        match s2.as_u16() {
            401 => println!("  401 on create → L1 signature does not recover to POLY_ADDRESS."),
            403 => println!("  403 on create → wallet is recognised but not permitted (region block?)."),
            404 => println!("  404 on create → endpoint path wrong for your cluster."),
            _ => {}
        }
        if b.contains("timestamp") || b2.contains("timestamp") {
            println!("  Possible clock skew — `sudo ntpdate pool.ntp.org` or equivalent.");
        }
        if b.contains("signature") || b2.contains("signature")
           || b.contains("address") || b2.contains("address") {
            println!("  Signature/address mismatch — verify POLYMARKET_PK corresponds to POLY_ADDRESS.");
        }
        println!("  If you've never used this wallet on polymarket.com, log in there first.");
        println!("  Compare the signature above against py-clob-client with the same ts/nonce.");

        Err(anyhow!("CLOB auth diagnostic failed"))
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

/// Intermediate values produced during L1 auth signing — useful for diagnostics.
#[derive(Debug, Clone)]
pub struct ClobAuthDebug {
    pub address:     String,
    pub timestamp:   i64,
    pub nonce:       u64,
    pub type_hash:   String,
    pub domain_sep:  String,
    pub struct_hash: String,
    pub digest:      String,
    pub signature:   String,
}

/// Shorten long response bodies for log output.
fn snip(s: &str) -> String {
    let one_line: String = s.chars().filter(|&c| c != '\n' && c != '\r').collect();
    if one_line.chars().count() <= 200 { one_line }
    else { one_line.chars().take(200).collect::<String>() + "…" }
}