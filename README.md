# btc5m-bot

A Rust TUI for trading Polymarket's 5-minute **Bitcoin Up or Down** prediction
markets. Colors the live Chainlink BTC/USD price green/red against each
window's opening "Price to Beat", shows both sides of the order book, your
current UP/DOWN positions with unrealized PnL, and lets you fire market or
limit orders with one keystroke.

## Design at a glance

```
┌─ BTC 5m Predictions ──────────────────────────────────────────────────┐
│ BTC/USD (Chainlink)  $  67,432.51   ▲   +32.51                        │
│ Price to Beat: $67,400.00   Closes in 02:34   Bitcoin Up or Down…     │
├───────────────────────────────────────────┬───────────────────────────┤
│ Order Book                                │ Positions                 │
│  UP price    size  │ DOWN price    size   │   UP                      │
│   0.62   ×   200   │  0.38   ×  200       │   120.00 sh @ 0.580       │
│   0.61   ×   150   │  0.39   ×  150       │          mark 0.620       │
│   ── 0.605 ──      │  ── 0.395 ──         │          uPnL $+4.80      │
│   0.60   ×   500   │  0.40   ×  500       │   DOWN                    │
│                                           │   Realized   $+12.00      │
│                                           │   Total      $+16.80      │
├───────────────────────────────────────────┴───────────────────────────┤
│ Fills                                                                 │
│ 20:23:15 BUY  UP   120.00  @ 0.580  = $69.60                          │
│ 20:18:42 SELL UP   100.00  @ 0.650  = $65.00  +7.00                   │
├───────────────────────────────────────────────────────────────────────┤
│ size [5.00] USDC   [u] buy UP  [d] buy DOWN  [U] sell UP  [D] sell …  │
│ › BUY 2.00 UP @ 0.620 ✓                                               │
└───────────────────────────────────────────────────────────────────────┘
```

## Architecture

```
┌────────────────────┐   ┌──────────────────────┐   ┌───────────────────┐
│  Crossterm keys    │──▶│                      │◀──│ Chainlink RTDS WS │
└────────────────────┘   │   mpsc<AppEvent>     │   └───────────────────┘
┌────────────────────┐   │       ↓              │   ┌───────────────────┐
│  Gamma poll (10s)  │──▶│    AppState          │◀──│ CLOB book WS      │
│  + market roll     │   │       ↓              │   │ (restart on roll) │
└────────────────────┘   │  ratatui::draw       │   └───────────────────┘
                         └──────────────────────┘
                                ↓ actions
                         ┌──────────────────────┐
                         │  TradingClient       │
                         │  alloy EIP-712 sign  │
                         │  reqwest → CLOB REST │
                         └──────────────────────┘
```

Four async tasks push into a single `mpsc<AppEvent>` channel. The main loop
drains events, mutates `AppState`, and redraws. Key events run through
`events::handle_key`, which returns a pure `Action` that the runtime then
dispatches on a worker task — no I/O happens on the render loop.

**Signing.** Orders are built against the on-chain `Order` struct from
[`ctf-exchange`](https://github.com/Polymarket/ctf-exchange) and signed with
EIP-712 using `alloy` + `alloy-sol-types`. L1 auth (`ClobAuthDomain` struct)
derives the L2 API credentials on first run; every subsequent REST call is
authed with HMAC-SHA256 over `ts + method + path + body`.

**Market discovery.** The Gamma API's `/events` endpoint is polled every 10s
with the `crypto-5m` tag filter. The bot picks the `btc-updown-5m-*` event
whose `[start_date, end_date)` contains the current UTC time and auto-rolls
when it closes.

**Price feed.** `wss://ws-live-data.polymarket.com` → topic
`crypto_prices_chainlink` → filter `symbol=btc/usd`. This is the exact
feed Polymarket resolves against, so the header color is consistent with
what decides your position.

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
cd btc5m-bot
cp .env.example .env
# ...edit .env with your keys
cargo build --release
```

### Run

```sh
RUST_LOG=btc5m_bot=debug ./target/release/btc5m-bot
```

## Key bindings

Normal mode:

| key     | action                                 |
|---------|----------------------------------------|
| `u` / `d` | **market BUY** UP / DOWN (FAK at best ask) |
| `U` / `D` | **market SELL** UP / DOWN (FAK at best bid) |
| `l`     | open limit-order modal                 |
| `c`     | cancel ALL open orders                 |
| `s`     | edit persistent ticket size            |
| `r`     | force-refresh active market            |
| `q` / `Esc` / `Ctrl-C` | quit                      |

Limit modal:

| key     | action                                 |
|---------|----------------------------------------|
| ← / →   | flip outcome (UP ↔ DOWN)               |
| ↑ / ↓   | flip side (BUY ↔ SELL)                 |
| `Tab`   | switch price / size field              |
| digits / `.` | edit current field                |
| `Enter` | submit as GTC limit order              |
| `Esc`   | cancel modal                           |

Size edit mode:

| key     | action                                 |
|---------|----------------------------------------|
| digits / `.` | edit size buffer                  |
| `Enter` / `Esc` | commit (reverts to default on parse fail) |

## What's intentionally *not* here

- **User channel WebSocket (authenticated fills).** Fills today are
  synthesised from successful order acks. For a tighter PnL, subscribe to
  `wss://ws-subscriptions-clob.polymarket.com/ws/user` with L2 auth and
  merge into `AppState` — same code path as book events.
- **On-chain allowance setting.** Assumed pre-approved; if not, run a
  one-time script to call `USDC.approve` and `CTF.setApprovalForAll` for the
  two Exchange contracts.
- **Winnings redemption.** When a market resolves, your winning shares sit
  in the Safe until you redeem via `CTF.redeemPositions`. Out of scope for
  this bot — the web UI handles it with one click.
- **Persistence.** Realized PnL resets on restart. Swap the `VecDeque<Fill>`
  for a sqlite table if you want history across sessions.

## Project layout

```
src/
├── main.rs            # tokio runtime, terminal init, action dispatch
├── config.rs          # env vars, endpoints, SignatureType enum
├── app.rs             # AppState, Position, Fill, event reducer
├── events.rs          # keyboard → Action (pure, unit-testable)
├── gamma.rs           # Gamma REST client, ActiveMarket
├── trading.rs         # EIP-712 Order sign + L1/L2 auth + CLOB POST
├── feeds/
│   ├── chainlink.rs   # RTDS WS → PriceTick
│   └── clob_ws.rs     # CLOB market WS → BookSnapshot
└── ui/
    └── render.rs      # full ratatui render in one function
```

## Safety

This bot signs orders on your behalf. Until you're confident in its
behaviour:

1. Start with `DEFAULT_SIZE_USDC=1.0` or `0.5`. Orders below ~$1 on
   Polymarket are often rejected by the CLOB minimum-order-size check —
   useful dry-run signal.
2. Keep `MARKET_SLIPPAGE_BPS` conservative; the FAK type won't fill worse
   than the visible best level, but consider an FOK for zero slippage.
3. Never check your private key into source control. The `.env.example`
   file ships with zeros specifically so that `cp .env.example .env` fails
   loudly if you forget to edit.

## License

MIT — do what you want, no warranty.
