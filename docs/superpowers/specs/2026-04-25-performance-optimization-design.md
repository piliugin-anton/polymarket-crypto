# Performance Optimization Design — 2026-04-25

## Goal

Eliminate all confirmed hot-path allocations and redundant work across the async event loop and the ratatui render path. Target: maximum sustained throughput at 20 Hz draw / high-frequency CLOB book ticks with zero unnecessary heap allocation on the critical path.

---

## Scope

13 targeted fixes across four files (`main.rs`, `app.rs`, `src/feeds/clob_ws.rs`, `src/ui/render.rs`) plus one `Cargo.toml` change. No architectural restructuring; the single-channel event loop and ratatui draw model are preserved.

---

## Section 1 — Async Event Loop (hot path)

### Fix 1 — Guard `user_bundle_tx.send` against unchanged bundles

**File:** `src/main.rs`

`build_user_ws_bundle(state)` is called and sent unconditionally after every non-key `AppEvent`. At 20 Hz book ticks this wakes the user-WS supervisor task ~20+ times per second even when the active market and trailing IDs have not changed.

**Fix:** Add `send_user_bundle_if_changed`, mirroring the existing `send_book_watch_if_changed`. Derive `PartialEq` and `Eq` on `UserWsBundle` and `UserWsMarket`. Only call `watch::Sender::send` when the new bundle differs from the last sent value.

```rust
fn send_user_bundle_if_changed(state: &AppState, tx: &watch::Sender<UserWsBundle>) {
    let next = build_user_ws_bundle(state);
    if *tx.borrow() == next { return; }
    let _ = tx.send(next);
}
```

### Fix 2 — Eliminate double `collect_book_watch_token_ids` per `Book` event

**Files:** `src/app.rs`, `src/main.rs`

`AppState::apply(Book)` calls `collect_book_watch_token_ids` at app.rs:896 to prune `watched_books`. Then `send_book_watch_if_changed` calls it again immediately after in `main.rs`. This is two `HashSet<String>` + `Vec<String>` allocations per book snapshot.

**Fix:** Have `apply(Book)` store the computed result in `self.cached_book_watch_tokens: Vec<String>` (a new field, kept sorted). `send_book_watch_if_changed` reads from that field instead of calling `collect_book_watch_token_ids`. One allocation per book tick instead of two.

### Fix 3 — Replace `Vec` linear scan with `HashSet` in `Book` handler

**File:** `src/app.rs` (line 897)

After `collect_book_watch_token_ids` the result is a `Vec<String>`, but it is immediately used for `watch.contains(&id_for_trail)` — a linear O(n) scan — then converted to a `HashSet` for `watched_books.retain`. 

**Fix:** Keep the intermediate representation as a `HashSet<String>` for the membership check and `retain` call. Convert to sorted `Vec` only for the watch-sender comparison (which requires `Vec` for the slice equality check in `send_book_watch_if_changed`). Since `collect_book_watch_token_ids` already builds a `HashSet` internally and then drains it into a `Vec`, expose a version that returns both, or restructure so the `HashSet` is used directly before sorting.

### Fix 4 — Coalesce `price_changes` snapshots per asset in `clob_ws`

**File:** `src/feeds/clob_ws.rs` (lines 216–234)

When a `MarketPriceChangesMsg` contains multiple items for the same `asset_id`, `send_snapshot` is called after each item — building a full `Vec<BookLevel>` and doing an async channel send per change. For a batch of 8 changes on the UP token, 8 snapshots are allocated and sent instead of 1.

**Fix:** Collect the set of distinct `asset_id`s touched in the batch, apply all changes first, then call `send_snapshot` once per touched asset:

```rust
let mut touched: HashSet<String> = HashSet::new();
for ch in &msg.price_changes {
    let aid = canonical_clob_token_id(&ch.asset_id);
    // apply change to BTreeMap...
    touched.insert(aid.into_owned());
}
for aid in &touched {
    let entry = books.get(aid).unwrap(); // already inserted above
    send_snapshot(aid, entry, &mut out_bids, &mut out_asks, tx).await;
}
```

Apply the same coalescing to the `Vec<RawEvent>` array path: after processing all events in the array, collect the set of `asset_id`s that received a `PriceChange`, then call `send_snapshot` once per distinct asset. `Book` (full snapshot) events are unaffected — they already reset the whole book and send once.

### Fix 5 — `canonical_clob_token_id` returns `Cow<'_, str>`

**File:** `src/trading.rs` (line 414)

Called on every `Book`, `UserChannelFill`, `OrderAck`, `TrailingExitDispatchDone`, and `RequestTrailingArm` event. For token IDs already in canonical decimal form (the common case after the first normalization at WS boundary), it allocates a new `String` unconditionally.

**Fix:** Change return type to `Cow<'_, str>`:

```rust
pub fn canonical_clob_token_id(s: &str) -> Cow<'_, str> {
    if let Some(u) = parse_clob_token_id(s) {
        Cow::Owned(u.to_string())
    } else {
        let trimmed = s.trim();
        if trimmed.len() == s.len() {
            Cow::Borrowed(s)
        } else {
            Cow::Owned(trimmed.to_string())
        }
    }
}
```

Callers that need an owned `String` call `.into_owned()`; callers that only need `&str` for a map lookup or comparison borrow directly. Update all call sites.

### Fix 6 — Move `reqwest_client()` outside the balance poll loop

**File:** `src/main.rs` (line 569)

`net::reqwest_client()` is called inside `loop { interval.tick().await; ... }`, rebuilding the entire connection pool (including TLS session cache) every 5 seconds.

**Fix:** Construct the client once before the loop:

```rust
let data_http = match net::reqwest_client() {
    Ok(h) => h,
    Err(e) => { debug!(error=%e, "..."); return; }
};
loop {
    interval.tick().await;
    // use data_http directly
}
```

### Fix 12 — Skip bundle/watch on `Tick` events

**File:** `src/main.rs`

`Tick` is a 1-Hz event that cannot change the market or trailing set. Currently it goes through the non-key branch which calls `build_user_ws_bundle` and `send_book_watch_if_changed`. Add `AppEvent::Tick` to the early-return branch alongside `AppEvent::Key`, or add an explicit guard so neither call runs for `Tick`.

---

## Section 2 — Render Path (per-frame allocations)

### Fix 7 — Cache computed display strings in `AppState`

**Files:** `src/app.rs`, `src/ui/render.rs`

Several helpers are called from `draw()` every frame and do heap work each time:

- `spot_pair_label()` — `format!("{}/USD", label)` on every frame
- `countdown_secs()` — `Utc::now()` syscall on every frame  
- Sentiment spans — rebuilt from scratch each frame in `draw_header_btc`

**Fix:** Add cached fields to `AppState`:

```rust
pub cached_pair_label: String,       // updated on StartTrading / MarketRoll
pub cached_countdown_secs: Option<i64>, // updated on AppEvent::Tick
pub cached_sentiment: SentimentDir,  // updated on Book / TopHoldersSentiment
```

Where `SentimentDir` is a simple `enum { Up, Down, Neutral, Unknown }`. `draw()` reads pre-computed values. `Utc::now()` moves from the render path to the 1-Hz `Tick` handler.

### Fix 8 — Replace `" ".repeat(gap)` with a static slice

**File:** `src/ui/render.rs` (line 206, `balance_row_right_aligned`)

Allocates a new `String` on every frame call to pad the balance row.

**Fix:**

```rust
const SPACES: &str = "                                "; // 32 spaces
// then:
let gap = gap.min(SPACES.len());
Span::raw(&SPACES[..gap])
```

Zero allocation. The balance panel inner width is at most ~28 chars.

### Fix 9 — O(1) `truncate` via byte-length short-circuit

**File:** `src/ui/render.rs` (line 988)

`s.chars().count()` scans the entire string even to determine it does not need truncating. All strings passed to `truncate` in this codebase (market question, status line) are ASCII.

**Fix:** Keep the `String` return type (ratatui's `Span::raw` requires `Into<Cow<'static, str>>`, so an owned `String` is needed). The real wins are:

1. Short-circuit on byte length — `if s.len() <= n { return s.to_string(); }` is O(1) for ASCII and covers the common (non-truncating) case.
2. When truncation is needed, use `char_indices` to find the exact byte boundary instead of `chars().take(n).collect::<String>()`, eliminating the intermediate iterator collection.

```rust
fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n { return s.to_string(); }          // O(1) fast path for ASCII
    match s.char_indices().nth(n.saturating_sub(1)) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}
```

### Fix 10 — Stack buffer in `fmt_money_decimals_inner`

**File:** `src/ui/render.rs` (line 955)

Three heap allocations per call: `w.to_string().into_bytes()` (alloc 1), `Vec::with_capacity` for comma digits (alloc 2), `format!` for the final string (alloc 3). Called up to ~10 times per frame for prices and balances.

**Fix:** Write directly into a `String::with_capacity(24)` in a single pass:

```rust
fn fmt_money_decimals_inner(v: f64, decimal_places: u32, floor_frac: bool) -> String {
    // compute whole + frac as before
    let mut out = String::with_capacity(24);
    // write sign, then digits with commas inserted every 3 from the right
    // write '.', then zero-padded frac
    out
}
```

One allocation, sized to fit the result without growing. Eliminates the `into_bytes` + intermediate Vec entirely.

---

## Section 3 — Miscellaneous

### Fix 11 — `==` instead of `clob_asset_ids_match` in trailing scans

**File:** `src/app.rs` (lines 471–477, `trailing_sell_queued_or_in_flight`)

`clob_asset_ids_match` parses both sides as `U256` on every comparison. Since all `token_id`s stored in `pending_trailing_sells` and `trailing_sell_in_flight` are normalized to canonical form at event entry points (via `canonical_clob_token_id`), a plain `==` comparison is sufficient.

**Fix:** Replace `clob_asset_ids_match(&e.token_id, token_id)` with `e.token_id == token_id` in the trailing scan paths. Add a `debug_assert!(token_id == canonical_clob_token_id(token_id).as_ref())` at entry points to verify the invariant in debug builds.

### Fix 13 — `panic = "abort"` in release profile

**File:** `Cargo.toml`

```toml
[profile.release]
opt-level        = 3
lto              = "thin"
codegen-units    = 1
strip            = true
panic            = "abort"   # add this
```

Removes stack-unwinding machinery. Safe for a single-binary CLI with no FFI boundary that needs to catch panics.

---

## Files Changed

| File | Fixes |
|------|-------|
| `src/main.rs` | 1, 2, 6, 12 |
| `src/app.rs` | 2, 3, 7, 11 |
| `src/feeds/clob_ws.rs` | 4 |
| `src/trading.rs` | 5 |
| `src/ui/render.rs` | 8, 9, 10 |
| `src/feeds/clob_user_ws.rs` | 1 (PartialEq derive on UserWsBundle) |
| `Cargo.toml` | 13 |

## Non-goals

- No change to the single-channel event loop architecture
- No `DisplayState` restructure (Approach C)
- No thread pinning
- No change to network topology or feed subscription logic
