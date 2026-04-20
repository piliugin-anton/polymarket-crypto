# Polymarket "Bitcoin Up or Down 5m" trading terminal

A Rust TUI for trading Polymarket's 5-minute **Bitcoin Up or Down** prediction
markets. Colors the live Chainlink BTC/USD price green/red against each
window's opening "Price to Beat", shows both sides of the order book, your
current UP/DOWN positions with unrealized PnL, and lets you fire **FAK**
market orders or **GTD** limit orders that auto-expire just before the
current 5m window closes — all from single-key actions.

## Architecture

```
┌─────────────────────┐   ┌───────────────────────────────┐   ┌────────────────────┐
│ Crossterm keys      │   │                               │   │ Chainlink RTDS WS  │
│ + resize / focus    │──▶│  mpsc<AppEvent> (bounded)     │◀──│ BTC/USD ticks      │
└─────────────────────┘   │  coalesce bursts (price/book) │   └────────────────────┘
┌─────────────────────┐   │            ↓                  │   ┌─────────────────────┐
│ 1 Hz Tick           │──▶│         AppState              │◀──│ CLOB market WS      │
└─────────────────────┘   │            ↓                  │   │ per-market book     │
┌─────────────────────┐   │     ratatui::draw             │   │ (supervisor restarts│
│ CLOB market WS      │──▶│  throttled ~20 Hz on feeds    │   │  on each roll)      │
│ `new_market` +      │   │                               │   └─────────────────────┘
│ Gamma fallback 60s  │   └───────────────┬───────────────┘
└─────────────────────┘                   │
         ┌────────────────────────────────┼────────────────────────────┐
         ▼                                ▼                            ▼
┌─────────────────┐              ┌──────────────────┐        ┌─────────────────────┐
│ Gamma REST      │              │ TradingClient    │        │ Data API (HTTP)     │
│ ActiveMarket    │              │ EIP-712 orders   │        │ positions index,    │
│ resolution      │              │ L1/L2 CLOB REST  │        │ neg-risk claimable  │
└─────────────────┘              └──────────────────┘        │ (roll bootstrap)    │
                                                             └──────────┬──────────┘
                                                                        │
                                                             ┌──────────▼──────────┐
                                                             │ Polygon JSON-RPC    │
                                                             │ (Multicall3         │
                                                             │  `aggregate3`)      │
                                                             │  panel cash + CTF)  │
                                                             └─────────────────────┘
```

Many async producers share one `mpsc<AppEvent>` channel (buffer 512): keyboard
and focus/resize handling, a 1&nbsp;Hz ticker, Chainlink price (via a small
forwarder that keeps only the latest tick per burst), CLOB book snapshots (via
a forwarder that merges concurrent UP/DOWN updates), market rolls, position /
open-order / balance snapshots, and order status lines. The main loop drains
events in batches, applies them to `AppState`, and calls `Terminal::draw`. When
a batch has **no** key events, redraws are **throttled** (`FEED_REDRAW_MIN`,
50&nbsp;ms) so feed-heavy sessions do not pin a CPU core — see ratatui
discussion around high-frequency `draw`.

Key events go through `events::handle_key`, which returns a pure `Action`. The
runtime dispatches trading, cancel, and **redeem all** (`x` / `X`) on separate
`tokio` tasks — **no** network I/O on the render path. On startup, API
credential derivation also runs in the background so the TUI can paint before
L2 auth completes.

**Signing.** Orders use the on-chain `Order` shape from
[`ctf-exchange`](https://github.com/Polymarket/ctf-exchange), signed with
EIP-712 (`alloy` + `alloy-sol-types`). L1 auth derives L2 credentials once;
later REST calls use HMAC-SHA256 over `ts + method + path + body`.

**Networking.** `net` builds a proxy-aware `reqwest` client and WebSocket
tunnels (`POLYMARKET_PROXY`: HTTP `CONNECT` or SOCKS5, then TLS + WS) shared by
Gamma, CLOB REST, Data API, RTDS, and both CLOB sockets. The **balance panel**
uses a **separate** HTTP client to `POLYGON_RPC_URL` only (no proxy) so
`eth_call` reads stay fast and are not routed through a Polymarket-blocked path.

**Market discovery.** A dedicated CLOB **market** WebSocket subscribes with an
empty `assets_ids` list and `custom_feature_enabled: true` so **global**
`new_market` events arrive (narrow token subscriptions miss the next 5&nbsp;m
window). When a slug looks like the current `btc-updown-5m-*` grid, the client
calls `GammaClient::find_current_btc_5m` to resolve a full [`ActiveMarket`](src/gamma.rs).
A **60&nbsp;s** Gamma poll runs as a fallback if the socket is quiet. A
supervisor task aborts the previous per-market book connection and starts a new
one on each roll; it also kicks off a positions sync (CLOB balances + `/data/trades`
replay, Data API sizes for escrowed sells) and a 5&nbsp;s open-order poller.

**Balances and claimable.** A 5&nbsp;s task reads **on-chain** values via Polygon
[Multicall3](https://github.com/mds1/multicall) **`aggregate3`** (one `eth_call`
per chunk): **USDC.e** cash (`balanceOf` on the bridged collateral token) and
**standard CTF** claimable from `payoutDenominator` / `payoutNumerators` +
ERC-1155 balances on the Conditional Tokens contract (see
[`balances.rs`](src/balances.rs)). The Data API lists **redeemable** markets
(standard) and supplies **neg-risk** claimable sums where position IDs differ.
**Redeem:** with `POLYMARKET_RELAYER_API_KEY` (+ `POLYMARKET_RELAYER_API_KEY_ADDRESS`)
set and `POLYMARKET_SIG_TYPE=2` (Gnosis Safe funder), **`x`** / **`X`** fetches
all redeemable rows from the Data API and submits **one** gasless Safe
`execTransaction` to the Polymarket relayer. Multiple distinct markets are
batched with Gnosis **`MultiSend`** + Safe **delegateCall** (same pattern as
[`@polymarket/builder-relayer-client`](https://github.com/Polymarket/builder-relayer-client));
a single market still uses a direct `redeemPositions` call on the CTF contract
or neg-risk adapter. Rows that cannot be built (bad ids, zero on-chain neg-risk
balances, etc.) are skipped with a log warning so the rest still redeem.

**Fees and take-profit.** `fees` implements Polymarket **crypto** taker fees for
PnL and for limit prices after optional **market BUY → GTD take-profit** sells
(`MARKET_BUY_TAKE_PROFIT_BPS`).

**Price feed.** `wss://ws-live-data.polymarket.com` → topic
`crypto_prices_chainlink` → filter `symbol=btc/usd` — the same feed used for
resolution, so the header matches settlement logic.

## Debugging

Every run writes a log to `./polymarket-btc5m.log` (override with `BTC5M_LOG_PATH`).
Default log level is `debug` — every HTTP response body, every WS subscribe
message, every EIP-712 digest is captured there. `tail -f polymarket-btc5m.log` in
another pane while the TUI runs.

If CLOB auth fails, the TUI's status line will scroll through the error
chain over ~10 seconds. For the definitive dump, quit the TUI and run:

```sh
./target/release/polymarket-btc5m debug-auth
```

That prints everything: the signer address, the funder, the proxy status,
the EIP-712 type hash, domain separator, struct hash, final digest, the
signature, then hits both `/auth/derive-api-key` and `/auth/api-key` and
shows the exact status + body you got back. If the digest looks right but
the server still says 401, the problem is usually one of:

- **Signer address doesn't match the wallet Polymarket expects** — check that your
  `POLYMARKET_PK` is for the right EOA. The `debug-auth` output shows the
  address derived from your key.
- **Clock skew** — `date -u` vs a known reference. Off by more than ~10s
  and the server rejects.
- **Wallet never set up on Polymarket** — log into polymarket.com with
  this EOA at least once to deploy the Safe; `create_or_derive_api_key`
  can't synthesise creds for a wallet the platform has never seen.
- **Proxy stripping headers** — some corporate/SOCKS proxies rewrite the
  `POLY_*` custom headers. Try a different proxy or a direct connection
  from a permitted region.

## Geo-restricted? Use a proxy

Polymarket blocks a broad set of IPs at the edge. If `cargo run` shows no BTC
price, no order book, and Gamma errors in the log, you're almost certainly
being blocked. Set `POLYMARKET_PROXY` in `.env` and everything — REST
(Gamma, CLOB REST) plus WebSockets (Chainlink RTDS, CLOB book) — will
tunnel through it:

```
POLYMARKET_PROXY=http://user:pass@proxy.example.com:8080   # HTTP(S)
POLYMARKET_PROXY=socks5://127.0.0.1:1080                   # SOCKS5
```

For HTTP proxies the bot sends a `CONNECT` for each WebSocket before doing
TLS + the WS handshake. For SOCKS5 it uses `tokio-socks` to do the
handshake and then treats the tunnel as a plain TCP stream. TLS (via
`rustls` with `webpki-roots`) happens at the origin so your proxy only sees
ciphertext. A residential or datacenter proxy in any non-blocked region
works — Polymarket only inspects the peer IP, not headers.

You'll see `proxy=…` in the startup log line when it's active.

## Troubleshooting CLOB credentials

If you see `could not derive CLOB API credentials` in the startup log, run:

```sh
./target/release/polymarket-btc5m debug-auth
```

This skips the TUI, runs the L1 auth flow with verbose output, and prints
every intermediate value (typeHash, domainSeparator, structHash, digest,
signature) alongside the actual HTTP status and body returned by
`GET /auth/derive-api-key` and `POST /auth/api-key`. Common patterns:

| what `debug-auth` shows | diagnosis |
|---|---|
| `401 Unauthorized` on both, body mentions *signature* or *address* | `ecrecover(digest, sig)` returned a different address from `POLY_ADDRESS`. Either the typeHash is wrong (shouldn't be after the v0.1 fix), or `POLYMARKET_PK` doesn't match the account you think it does. |
| `401 Unauthorized`, body mentions *timestamp* | clock skew > ~10 s. Run `sudo ntpdate pool.ntp.org` (or `w32tm /resync` on Windows). |
| `403 Forbidden` on create | proxy is in a region Polymarket still blocks, or the wallet tripped their compliance layer. Try a different proxy region. |
| `404 Not Found` on derive, `200 OK` on create | your wallet had no prior API keys; now it does. The TUI will work on next launch. |
| `200 OK` on derive but TUI still errors | the creds are fine — the next failure is probably L2 (HMAC), likely a base64 decoding mismatch on the secret. |
| `warning: POLYMARKET_FUNDER equals your signer…` | misconfigured `sig_type`. EOA wallets need `sig_type=0` and `funder=eoa`. Safe users need `sig_type=2` and `funder=safe_address`. |

**Wallet never used on polymarket.com**: if this EOA/Safe has never placed a
trade through the web UI, the backend may have no record of it and will
reject API-key creation with a 403. Log in once at polymarket.com with the
same wallet, deposit $1 USDC, then re-run `debug-auth`.

**Comparing against py-clob-client**: the gold standard for verification is
to point the Python client at the same wallet with the same timestamp and
nonce, then compare the resulting `POLY_SIGNATURE` byte-for-byte against
what `debug-auth` prints. If they differ, the issue is in our signing; if
they match and Python works but Rust doesn't, the issue is in our HTTP
layer (headers, proxy, TLS).

## Setup

### Prerequisites

- Rust **1.80+** (`rustup toolchain install stable`)
- A funded Polymarket wallet. For the typical UX that means an EOA that owns
  a Gnosis Safe holding your USDC. Both addresses go in `.env`.
- The Safe must have already approved the CTF Exchange + NegRisk Exchange as
  spenders — if you've ever placed a trade through the web UI, this is
  already done. If not, see the
  [NautilusTrader setup script](https://nautilustrader.io/docs/latest/integrations/polymarket/)
  for reference allowance-setting code.

### Install

```sh
git clone <your-fork>
cd polymarket-btc5m
cp .env.example .env
# ...edit .env with your keys (incl. POLYGON_RPC_URL for on-chain balance reads)
cargo build --release
```

Set **`POLYGON_RPC_URL`** to a reliable Polygon HTTPS endpoint (Alchemy, drpc,
public `polygon-rpc.com`, etc.). The TUI balance panel does **not** use
`POLYMARKET_PROXY` for this URL. If you see empty Cash/Claimable, verify the URL
and that the process was restarted after editing `.env`.

### Run

```sh
RUST_LOG=polymarket-btc5m=debug ./target/release/polymarket-btc5m
```

## Key bindings

Normal mode:

| key     | action                                 |
|---------|----------------------------------------|
| `u` / `d` | **market BUY** UP / DOWN (FAK at best ask) |
| `U` / `D` | **market SELL** UP / DOWN (FAK at best bid) |
| `l`     | open limit-order modal                 |
| `c`     | cancel ALL open orders                 |
| `x` / `X` | **redeem all** claimable resolved positions (relayer + Safe; see *Balances and claimable*) |
| `s`     | edit persistent ticket size            |
| `r`     | force-refresh active market            |
| `q` / `Esc` / `Ctrl-C` | quit                    |

**Sizing (Polymarket CLOB).** Per [Create order](https://docs.polymarket.com/developers/CLOB/orders/create-order), **FAK/FOK market BUY** is a **USDC dollar budget** (“specify the dollar amount you want to spend”); **market SELL** is **outcome shares**. **GTC/GTD limit BUY** uses **`size` in shares** at your limit price (this TUI’s limit modal still types BUY size as USDC notional, then converts to shares before submit). The `minimum_order_size` / `min_order_size` fields on [`getOrderBook`](https://docs.polymarket.com/developers/CLOB/clients/methods-public#getOrderBook) are **share** thresholds— they align with **limit** flow and **share-sized** legs, **not** a direct “\$5 minimum spend” on **market BUY**. Small **market BUY** tickets (e.g. **\$1** while the ask is **> 0.5**, i.e. fewer than five shares if fully filled at that price) can still match in practice; third-party guides often cite **~\$1** as a **market** floor and **~5 shares** for **limits** (e.g. [Start Polymarket — How to Trade](https://startpolymarket.com/guides/how-to-trade/)). If placement fails with `INVALID_ORDER_MIN_SIZE`, increase size or re-check book metadata for that token.

Limit modal:

| key     | action                                 |
|---------|----------------------------------------|
| ← / →   | flip outcome (UP ↔ DOWN)               |
| ↑ / ↓   | flip side (BUY ↔ SELL)                 |
| `Tab`   | switch price / size field              |
| digits / `.` | edit current field                |
| `Enter` | submit as **GTD** limit order          |
| `Esc`   | cancel modal                           |

The modal enforces at least **5 outcome shares** on submit after converting BUY notional → shares (SELL: size is already shares). That matches typical **`minimum_order_size`** on **btc-updown-5m-*** books and avoids `INVALID_ORDER_MIN_SIZE` on **GTD**; see `min_order_size` / `minimum_order_size` in Polymarket’s [order book](https://docs.polymarket.com/developers/CLOB/clients/methods-public#getOrderBook) / [market](https://docs.polymarket.com/developers/CLOB/clients/methods-public#getMarket) payloads (values can differ by slug).

GTD expiration is chosen so the order stops resting about **one second before** the active market’s `closes_at`. The CLOB expects a unix `expiration` field with Polymarket’s **+60s** security buffer on top of that instant (see [Create order → GTD](https://docs.polymarket.com/developers/CLOB/orders/create-order)). **CLOB API signing version must be 1** (EIP-712 includes `expiration`); if `/version` returns `2`, GTD placement is rejected until the client supports it.

Size edit mode:

| key     | action                                 |
|---------|----------------------------------------|
| digits / `.` | edit size buffer                  |
| `Enter` / `Esc` | commit (reverts to default on parse fail) |
| `x` / `X` | same as normal mode: **redeem all** (exits size edit first) |

## What's intentionally *not* here

- **User channel WebSocket (authenticated fills).** Fills today are
  synthesised from successful order acks. For a tighter PnL, subscribe to
  `wss://ws-subscriptions-clob.polymarket.com/ws/user` with L2 auth and
  merge into `AppState` — same code path as book events.
- **On-chain allowance setting.** Assumed pre-approved; if not, run a
  one-time script to call `USDC.approve` and `CTF.setApprovalForAll` for the
  two Exchange contracts.
- **Winnings redemption without relayer keys.** **`x`** batches redeemable
  markets through the relayer only when `POLYMARKET_RELAYER_API_KEY` and
  `POLYMARKET_RELAYER_API_KEY_ADDRESS` are set (Safe / `sig_type=2`). Otherwise
  use the web Portfolio **Claim** flow or another tool.
- **Persistence.** Realized PnL resets on restart. Swap the `VecDeque<Fill>`
  for a sqlite table if you want history across sessions.

## Project layout

```
src/
├── main.rs                 # tokio runtime, TUI init, event loop, action dispatch
├── config.rs               # env vars, endpoints, SignatureType
├── app.rs                  # AppState, positions, fills, AppEvent reducer
├── events.rs               # keyboard → Action (pure)
├── gamma.rs                # Gamma REST, ActiveMarket, GTD expiration helper
├── trading.rs              # EIP-712 orders, L1/L2 auth, CLOB REST
├── balances.rs             # On-chain USDC.e + CTF claimable (Polygon Multicall3)
├── data_api.rs             # Data API: positions, redeemable index, neg-risk
├── redeem.rs               # CTF redeem via Polymarket relayer (Safe)
├── fees.rs                 # crypto taker fee + take-profit limit price
├── net.rs                  # proxy-aware HTTP + WebSocket connect
├── feeds/
│   ├── chainlink.rs        # RTDS WS → PriceTick
│   ├── clob_ws.rs          # per-market CLOB WS → BookSnapshot
│   └── market_discovery_ws.rs  # global new_market + Gamma fallback
└── ui/
    └── render.rs           # ratatui layout
```

## Safety

This bot signs orders on your behalf. Until you're confident in its
behaviour:

1. Start with a small `DEFAULT_SIZE_USDC`. **`u` / `d` market BUY** uses that
   value as a **USDC spend** budget (Polymarket **FAK BUY** = dollars; see
   [Create order → Order types](https://docs.polymarket.com/developers/CLOB/orders/create-order)),
   so **\$1** tickets can fill even when the implied share count is **below**
   book **`minimum_order_size`** at the current ask. **GTD limits** are blocked
   in-app below **5 shares** after notional→share conversion; you can still see
   `INVALID_ORDER_MIN_SIZE` or liquidity errors from the API for edge sizes—
   bump the ticket or check the book if that happens.
2. Tune `MARKET_BUY_SLIPPAGE_BPS` / `MARKET_SELL_SLIPPAGE_BPS` (use `0` for no
   cushion); legacy `MARKET_SLIPPAGE_BPS` still sets either side if unset.
3. Never check your private key into source control. The `.env.example`
   file ships with zeros specifically so that `cp .env.example .env` fails
   loudly if you forget to edit.

## License

MIT — do what you want, no warranty.