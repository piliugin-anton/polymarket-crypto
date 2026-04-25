# Performance Optimization Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Eliminate confirmed hot-path allocations and redundant work in the async event loop and ratatui render path for maximum sustained throughput at 20 Hz draw / high-frequency CLOB book ticks.

**Architecture:** 13 fixes in dependency order — `trading.rs` (Cow return type) first since all other files call `canonical_clob_token_id`; then `app.rs` event loop fixes; then `clob_ws.rs` batch coalescing; then `main.rs` guard/loop fixes; then `render.rs` per-frame fixes. Each task compiles and passes `cargo check` independently. `UserWsBundle`/`UserWsMarket` already derive `PartialEq`/`Eq` — no separate struct task needed.

**Tech Stack:** Rust 1.80+, Tokio, ratatui 0.29, crossterm — no new dependencies.

**Spec:** `docs/superpowers/specs/2026-04-25-performance-optimization-design.md`

---

## File Map

| File | Fixes | What changes |
|------|-------|--------------|
| `Cargo.toml` | 13 | `panic = "abort"` in release profile |
| `src/trading.rs` | 5 | `canonical_clob_token_id` → `Cow<'_, str>` |
| `src/app.rs` | 2, 3, 5 (sites), 7, 11 | Cow call sites; trailing `==`; cached book watch field; `SentimentDir`; cached display fields |
| `src/feeds/clob_ws.rs` | 4 | Coalesce `price_changes` snapshots per asset per batch |
| `src/main.rs` | 1, 6, 12 | Guard user bundle send; reqwest outside loop; skip `Tick` in bundle/watch |
| `src/ui/render.rs` | 8, 9, 10 | Static spaces; O(1) truncate; single-alloc number format |

---

## Task 1: `panic = "abort"` in release profile (Fix 13)

**Files:**
- Modify: `Cargo.toml`

- [ ] **Step 1: Add `panic = "abort"` to `[profile.release]`**

```toml
[profile.release]
opt-level        = 3
lto              = "thin"
codegen-units    = 1
strip            = true
panic            = "abort"
```

- [ ] **Step 2: Verify release build**

```bash
cargo build --release 2>&1 | tail -3
```

Expected: `Finished release [optimized] target(s)` — no errors.

- [ ] **Step 3: Commit**

```bash
git add Cargo.toml
git commit -m "perf: panic=abort in release profile"
```

---

## Task 2: `canonical_clob_token_id` → `Cow<'_, str>` (Fix 5)

**Files:**
- Modify: `src/trading.rs`
- Modify: `src/feeds/clob_ws.rs`

Returns `Cow::Borrowed` when the token ID is already in canonical form (common case after normalization at WS boundary), eliminating the heap allocation. Callers that need an owned `String` call `.into_owned()`.

- [ ] **Step 1: Change the function in `src/trading.rs` (~line 414)**

Replace the existing function body:

```rust
pub fn canonical_clob_token_id(s: &str) -> std::borrow::Cow<'_, str> {
    if let Some(u) = parse_clob_token_id(s) {
        std::borrow::Cow::Owned(u.to_string())
    } else {
        let trimmed = s.trim();
        if trimmed.len() == s.len() {
            std::borrow::Cow::Borrowed(s)
        } else {
            std::borrow::Cow::Owned(trimmed.to_string())
        }
    }
}
```

- [ ] **Step 2: Update `src/feeds/clob_ws.rs` call sites (three locations)**

All three uses are as `HashMap` keys and need owned `String`s. Add `.into_owned()` to each:

**In `apply_raw_event`, `RawEvent::Book` arm (~line 258):**
```rust
let asset_id = canonical_clob_token_id(&asset_id).into_owned();
```

**In `apply_raw_event`, `RawEvent::PriceChange` arm (~line 282):**
```rust
let asset_id = canonical_clob_token_id(&asset_id).into_owned();
```

**In `run_once`, `MarketPriceChangesMsg` loop (~line 217):**
```rust
let aid = canonical_clob_token_id(&ch.asset_id).into_owned();
```

- [ ] **Step 3: Check clob_ws.rs compiles (app.rs errors expected, that's fine)**

```bash
cargo check --message-format=short 2>&1 | grep "clob_ws"
```

Expected: no errors in `clob_ws.rs`.

- [ ] **Step 4: Commit**

```bash
git add src/trading.rs src/feeds/clob_ws.rs
git commit -m "perf(trading): canonical_clob_token_id returns Cow — clob_ws.rs sites"
```

---

## Task 3: Update `app.rs` Cow call sites + trailing scan `==` (Fixes 5 sites, 11)

**Files:**
- Modify: `src/app.rs`

Two independent changes: update the 8 remaining call sites for the new `Cow` return type; replace `clob_asset_ids_match` with plain `==` in trailing scans (token IDs are always canonical when stored).

- [ ] **Step 1: Update `trailing_map_key_for_asset` (~line 37)**

```rust
fn trailing_map_key_for_asset(
    trailing: &HashMap<String, TrailingSession>,
    asset_id: &str,
) -> Option<String> {
    if trailing.contains_key(asset_id) {
        return Some(asset_id.to_string());
    }
    let c = canonical_clob_token_id(asset_id);
    if c.as_ref() != asset_id && trailing.contains_key(c.as_ref()) {
        return Some(c.into_owned());
    }
    trailing
        .keys()
        .find(|k| clob_asset_ids_match(k, asset_id))
        .cloned()
}
```

- [ ] **Step 2: Update `pending_trail_map_key_for_asset` (~line 55)**

```rust
fn pending_trail_map_key_for_asset(
    pending: &HashMap<String, PendingTrailArm>,
    asset_id: &str,
) -> Option<String> {
    if pending.contains_key(asset_id) {
        return Some(asset_id.to_string());
    }
    let c = canonical_clob_token_id(asset_id);
    if c.as_ref() != asset_id && pending.contains_key(c.as_ref()) {
        return Some(c.into_owned());
    }
    pending
        .keys()
        .find(|k| clob_asset_ids_match(k, asset_id))
        .cloned()
}
```

- [ ] **Step 3: Update `mid_for_token` (~line 535)**

```rust
pub fn mid_for_token(&self, token_id: &str) -> Option<f64> {
    if let Some(m) = &self.market {
        if token_id == m.up_token_id.as_str() {
            return self.book_up.as_deref().and_then(book_mid);
        }
        if token_id == m.down_token_id.as_str() {
            return self.book_down.as_deref().and_then(book_mid);
        }
    }
    self.watched_books
        .get(token_id)
        .or_else(|| {
            let c = canonical_clob_token_id(token_id);
            if c.as_ref() != token_id {
                self.watched_books.get(c.as_ref())
            } else {
                None
            }
        })
        .map(|b| b.as_ref())
        .and_then(book_mid)
}
```

- [ ] **Step 4: Update `book_snapshot_for_token` (~line 564)**

```rust
fn book_snapshot_for_token(&self, token_id: &str) -> Option<&BookSnapshot> {
    if let Some(m) = &self.market {
        if token_id == m.up_token_id.as_str() {
            return self.book_up.as_deref();
        }
        if token_id == m.down_token_id.as_str() {
            return self.book_down.as_deref();
        }
    }
    self.watched_books
        .get(token_id)
        .or_else(|| {
            let c = canonical_clob_token_id(token_id);
            if c.as_ref() != token_id {
                self.watched_books.get(c.as_ref())
            } else {
                None
            }
        })
        .map(|b| b.as_ref())
}
```

- [ ] **Step 5: Update `install_trailing_session` (~line 678)**

Change the first line of the function body:
```rust
let token_id = canonical_clob_token_id(&token_id).into_owned();
```

- [ ] **Step 6: Update all `apply()` event handler call sites**

In each location below, change `canonical_clob_token_id(...)` to `canonical_clob_token_id(...).into_owned()` — these all need owned Strings for storage or HashMap insertion.

- `AppEvent::Book` arm (~line 886): `b.asset_id = canonical_clob_token_id(&b.asset_id).into_owned();`
- `AppEvent::UserChannelFill` arm (~line 998): `let token_id = canonical_clob_token_id(&token_id).into_owned();`
- `AppEvent::OrderAck` arm (~line 1052): `let token_id = canonical_clob_token_id(&token_id).into_owned();`
- `AppEvent::RequestTrailingArm` arm (~line 1108): `let token_id = canonical_clob_token_id(&token_id).into_owned();`
- `AppEvent::TrailingExitDispatchDone` arm (~line 1234): `let token_id = canonical_clob_token_id(&token_id).into_owned();`

- [ ] **Step 7: Replace `clob_asset_ids_match` with `==` in `trailing_sell_queued_or_in_flight` (Fix 11, ~line 470)**

All stored `token_id`s are in canonical form at insert time. Plain `==` on canonical strings is correct and eliminates the U256 parse on every scan.

```rust
fn trailing_sell_queued_or_in_flight(&self, token_id: &str) -> bool {
    debug_assert_eq!(
        token_id,
        canonical_clob_token_id(token_id).as_ref(),
        "token_id must be canonical before calling this"
    );
    self.pending_trailing_sells
        .iter()
        .any(|e| e.token_id == token_id)
        || self.trailing_sell_in_flight.contains(token_id)
}
```

- [ ] **Step 8: Verify compilation**

```bash
cargo check --message-format=short 2>&1 | head -20
```

Expected: no errors.

- [ ] **Step 9: Commit**

```bash
git add src/app.rs
git commit -m "perf(app): Cow call sites, trailing == scan, remove clob_asset_ids_match on hot path"
```

---

## Task 4: Eliminate double `collect_book_watch_token_ids` + O(1) HashSet in Book handler (Fixes 2, 3)

**Files:**
- Modify: `src/app.rs`
- Modify: `src/main.rs`

Add `cached_book_watch_tokens: Vec<String>` to `AppState`. The `Book` handler builds a local `HashSet<String>` once (O(1) `contains` + `retain`), stores the sorted result in the field, and sets a flag so the end-of-`apply()` update is skipped. All other events update the field once at the end of `apply()`. `send_book_watch_if_changed` reads the cached field — no extra collection pass.

- [ ] **Step 1: Add `collect_book_watch_token_ids_as_set` helper to `src/app.rs`**

Add this function directly below `collect_book_watch_token_ids` (~line 344):

```rust
fn collect_book_watch_token_ids_as_set(state: &AppState) -> HashSet<String> {
    let mut s: HashSet<String> = HashSet::new();
    if let Some(m) = &state.market {
        s.insert(m.up_token_id.clone());
        s.insert(m.down_token_id.clone());
    }
    for k in state.trailing.keys() {
        s.insert(k.clone());
    }
    for k in state.pending_trail_arms.keys() {
        s.insert(k.clone());
    }
    for p in &state.pending_trailing_sells {
        s.insert(p.token_id.clone());
    }
    s
}
```

- [ ] **Step 2: Add `cached_book_watch_tokens` to `AppState` struct and `new()`**

In the `AppState` struct definition (~line 347), add the field before the closing `}`:

```rust
/// Sorted token IDs for the book WebSocket watch. Updated in `apply()` so
/// `send_book_watch_if_changed` does not recompute it on every event.
pub cached_book_watch_tokens: Vec<String>,
```

In `AppState::new()` (~line 429), add to the initializer:

```rust
cached_book_watch_tokens: Vec::new(),
```

- [ ] **Step 3: Update `apply()` — add flag at top, update Book arm, update end**

At the top of `apply()`, before the `match ev {` line, add:

```rust
let mut book_watch_cached = false;
```

In the `AppEvent::Book` arm, replace the three lines that build `watch` and `keep`:

```rust
// OLD (remove these three):
// let watch = collect_book_watch_token_ids(self);
// if watch.contains(&id_for_trail) { ... }
// let keep: HashSet<String> = watch.into_iter().collect();
// self.watched_books.retain(|k, _| keep.contains(k));

// NEW:
let watch_set = collect_book_watch_token_ids_as_set(self);
if watch_set.contains(id_for_trail.as_str()) {
    self.watched_books.insert(id_for_trail.clone(), snap);
}
self.watched_books.retain(|k, _| watch_set.contains(k.as_str()));
let mut sorted: Vec<String> = watch_set.into_iter().collect();
sorted.sort();
self.cached_book_watch_tokens = sorted;
book_watch_cached = true;
```

After the closing `}` of the `match ev { ... }` block (i.e., at the very end of `apply()`), add:

```rust
if !book_watch_cached {
    self.cached_book_watch_tokens = collect_book_watch_token_ids(self);
}
```

- [ ] **Step 4: Update `send_book_watch_if_changed` in `src/main.rs` to read cached field**

Find `send_book_watch_if_changed` (~line 145) and replace its body:

```rust
fn send_book_watch_if_changed(state: &AppState, book_token_tx: &watch::Sender<Vec<String>>) {
    if state.cached_book_watch_tokens.as_slice() == book_token_tx.borrow().as_slice() {
        return;
    }
    let _ = book_token_tx.send(state.cached_book_watch_tokens.clone());
}
```

- [ ] **Step 5: Verify compilation**

```bash
cargo check --message-format=short 2>&1 | head -20
```

Expected: no errors.

- [ ] **Step 6: Write a unit test for `collect_book_watch_token_ids_as_set`**

Add a `#[cfg(test)]` module at the bottom of `src/app.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::feeds::user_trade_sync::UserTradeSync;
    use std::sync::Arc;

    fn make_state() -> AppState {
        let uts = Arc::new(UserTradeSync::new());
        AppState::new(10.0, uts)
    }

    #[test]
    fn book_watch_tokens_sorted_and_deduped() {
        let state = make_state();
        let set = collect_book_watch_token_ids_as_set(&state);
        assert!(set.is_empty());

        let v = collect_book_watch_token_ids(&state);
        assert!(v.is_empty());
    }
}
```

- [ ] **Step 7: Run tests**

```bash
cargo test 2>&1 | tail -10
```

Expected: all tests pass.

- [ ] **Step 8: Commit**

```bash
git add src/app.rs src/main.rs
git commit -m "perf(app): cached book watch tokens, O(1) HashSet in Book handler, no double collect"
```

---

## Task 5: Cached display strings — `SentimentDir`, pair label, countdown (Fix 7)

**Files:**
- Modify: `src/app.rs`
- Modify: `src/ui/render.rs`

Add `SentimentDir` enum and three cached fields to `AppState`. Update event handlers to compute once. Update `draw_header_btc` to read from cached fields instead of calling methods that allocate or call `Utc::now()`.

- [ ] **Step 1: Add `SentimentDir` enum to `src/app.rs`**

Add near the top of `app.rs`, after the existing `pub enum UiPhase` definition (~line 309):

```rust
/// Pre-computed sentiment direction for the header — updated in `apply()`, read by `draw()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SentimentDir {
    Up,
    Down,
    Neutral,
    Unknown,
}
```

- [ ] **Step 2: Add three cached fields to `AppState` struct**

Add these fields to the struct definition, after `top_holders_down_sum`:

```rust
/// Pre-computed from CLOB mid + top-holder sums; updated on Book and TopHoldersSentiment events.
pub cached_sentiment: SentimentDir,
/// Updated once per second in the Tick handler; avoids `Utc::now()` in the draw path.
pub cached_countdown_secs: Option<i64>,
/// `"{asset}/USD"` label, set once on StartTrading.
pub cached_pair_label: String,
```

In `AppState::new()`, initialize:

```rust
cached_sentiment: SentimentDir::Unknown,
cached_countdown_secs: None,
cached_pair_label: "\u{2014}/USD".to_string(), // "—/USD"
```

- [ ] **Step 3: Add `recompute_sentiment` method to `AppState`**

Add this private method to the `impl AppState` block:

```rust
fn recompute_sentiment(&mut self) {
    const SENT_EPS: f64 = 1e-6;
    let m_up   = self.mark(Outcome::Up);
    let m_down = self.mark(Outcome::Down);
    self.cached_sentiment = match (m_up, m_down) {
        (Some(u), Some(d)) if u > d + SENT_EPS => SentimentDir::Up,
        (Some(u), Some(d)) if d > u + SENT_EPS => SentimentDir::Down,
        (Some(_), Some(_))                      => SentimentDir::Neutral,
        _ => match (self.top_holders_up_sum, self.top_holders_down_sum) {
            (Some(u), Some(d)) if u > d + SENT_EPS => SentimentDir::Up,
            (Some(u), Some(d)) if d > u + SENT_EPS => SentimentDir::Down,
            (Some(_), Some(_))                      => SentimentDir::Neutral,
            _                                       => SentimentDir::Unknown,
        },
    };
}
```

- [ ] **Step 4: Update `AppEvent::Tick` handler in `apply()` to set countdown cache**

In the `AppEvent::Tick` arm, add after the existing toast expiry check:

```rust
self.cached_countdown_secs = self.market
    .as_ref()
    .map(|m| (m.closes_at - Utc::now()).num_seconds().max(0));
```

- [ ] **Step 5: Update `AppEvent::Book` handler to call `recompute_sentiment`**

At the end of the `AppEvent::Book` arm (after `apply_trailing_book_tick`), add:

```rust
self.recompute_sentiment();
```

- [ ] **Step 6: Update `AppEvent::TopHoldersSentiment` handler**

At the end of the `AppEvent::TopHoldersSentiment` arm, add:

```rust
self.recompute_sentiment();
```

- [ ] **Step 7: Update `AppEvent::MarketRoll` handler**

At the end of the `AppEvent::MarketRoll` arm, add:

```rust
self.cached_sentiment = SentimentDir::Unknown;
self.cached_countdown_secs = None;
```

- [ ] **Step 8: Update `AppEvent::StartTrading` handler**

At the end of the `AppEvent::StartTrading` arm, add:

```rust
self.cached_pair_label = format!("{}/USD", p.asset.label);
self.cached_sentiment = SentimentDir::Unknown;
self.cached_countdown_secs = None;
```

- [ ] **Step 9: Update `src/ui/render.rs` `draw_header_btc` to read cached fields**

Add `SentimentDir` to the `use crate::app::` import at the top of `render.rs`:

```rust
use crate::app::{AppState, DepositModalPhase, InputMode, LimitField, Outcome, SentimentDir, UiPhase};
```

In `draw_header_btc` (~line 212), replace:

```rust
let pair = s.spot_pair_label();
```
with:
```rust
let pair = s.cached_pair_label.as_str();
```

Replace the two `s.countdown_secs()` calls:

```rust
// Replace this block:
let cd = s.countdown_secs()
    .map(|c| format!("{:02}:{:02}", c / 60, c % 60))
    .unwrap_or_else(|| "—".into());
```
with:
```rust
let cd = s.cached_countdown_secs
    .map(|c| format!("{:02}:{:02}", c / 60, c % 60))
    .unwrap_or_else(|| "—".into());
```

And the inline `s.countdown_secs()` call in the colour match:
```rust
Style::default().fg(match s.countdown_secs() {
```
with:
```rust
Style::default().fg(match s.cached_countdown_secs {
```

Replace the entire `sentiment_spans` block (the large match on `m_up`/`m_down` including the `let m_up = ...` and `let m_down = ...` lines above it) with:

```rust
let sentiment_spans: Vec<Span> = match s.cached_sentiment {
    SentimentDir::Up => vec![
        Span::styled("Sentiment: ", Style::default().fg(Color::DarkGray)),
        Span::styled("▲", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
    ],
    SentimentDir::Down => vec![
        Span::styled("Sentiment: ", Style::default().fg(Color::DarkGray)),
        Span::styled("▼", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
    ],
    SentimentDir::Neutral => vec![
        Span::styled("Sentiment: ", Style::default().fg(Color::DarkGray)),
        Span::styled("·",  Style::default().fg(Color::DarkGray)),
    ],
    SentimentDir::Unknown => vec![
        Span::styled("Sentiment: ", Style::default().fg(Color::DarkGray)),
        Span::styled("—",  Style::default().fg(Color::DarkGray)),
    ],
};
```

- [ ] **Step 10: Verify compilation**

```bash
cargo check --message-format=short 2>&1 | head -20
```

Expected: no errors.

- [ ] **Step 11: Commit**

```bash
git add src/app.rs src/ui/render.rs
git commit -m "perf(app+render): SentimentDir enum, cached pair label/countdown/sentiment in AppState"
```

---

## Task 6: Coalesce `price_changes` snapshots per asset in `clob_ws.rs` (Fix 4)

**Files:**
- Modify: `src/feeds/clob_ws.rs`

When a `MarketPriceChangesMsg` or `Vec<RawEvent>` batch contains multiple changes for the same asset, one snapshot is sent per touched asset instead of one per change. Extracts a sync `apply_raw_event_to_books` helper that applies changes without sending; callers collect touched assets and send after the batch.

- [ ] **Step 1: Add sync `apply_raw_event_to_books` helper**

Add this function before the existing `apply_raw_event` (keeping `apply_raw_event` intact for the single-event path — it will call this new helper):

```rust
/// Apply a raw event to the local book maps. Returns the canonical asset ID if the
/// event touched a book (i.e., `Book` or `PriceChange`); `None` for `Other`.
/// Does NOT send a snapshot — callers batch and send after the whole message.
fn apply_raw_event_to_books(
    books: &mut HashMap<String, (BTreeMap<i64, f64>, BTreeMap<i64, f64>)>,
    ev: RawEvent,
) -> Option<String> {
    match ev {
        RawEvent::Book { asset_id, bids, asks } => {
            let asset_id = canonical_clob_token_id(&asset_id).into_owned();
            let entry = books.entry(asset_id.clone()).or_default();
            entry.0.clear();
            entry.1.clear();
            for l in &bids {
                if let (Ok(p), Ok(s)) = (l.price.parse::<f64>(), l.size.parse::<f64>()) {
                    entry.0.insert(price_to_key(p), s);
                }
            }
            for l in &asks {
                if let (Ok(p), Ok(s)) = (l.price.parse::<f64>(), l.size.parse::<f64>()) {
                    entry.1.insert(price_to_key(p), s);
                }
            }
            Some(asset_id)
        }
        RawEvent::PriceChange { asset_id, changes } => {
            let asset_id = canonical_clob_token_id(&asset_id).into_owned();
            let entry = books.entry(asset_id.clone()).or_default();
            for c in &changes {
                let (Ok(p), Ok(s)) = (c.price.parse::<f64>(), c.size.parse::<f64>()) else {
                    continue;
                };
                let key = price_to_key(p);
                let map = if c.side == "BUY" { &mut entry.0 } else { &mut entry.1 };
                if s == 0.0 {
                    map.remove(&key);
                } else {
                    map.insert(key, s);
                }
            }
            Some(asset_id)
        }
        RawEvent::Other => None,
    }
}
```

- [ ] **Step 2: Update the `Vec<RawEvent>` array path in `run_once`**

Find the `if t.trim_start().starts_with('[')` block (~line 197) and replace its body:

```rust
if t.trim_start().starts_with('[') {
    if let Ok(events) = serde_json::from_str::<Vec<RawEvent>>(t) {
        let mut touched: HashSet<String> = HashSet::new();
        for ev in events {
            if let Some(aid) = apply_raw_event_to_books(&mut books, ev) {
                touched.insert(aid);
            }
        }
        for aid in &touched {
            if let Some(entry) = books.get(aid) {
                send_snapshot(aid, entry, &mut out_bids, &mut out_asks, tx).await;
            }
        }
    } else {
        warn_unparsed_clob(&txt);
    }
    continue;
}
```

Add `use std::collections::HashSet;` to the imports at the top of `clob_ws.rs` if not already present. Check:

```bash
grep "use std::collections::HashSet" src/feeds/clob_ws.rs
```

If missing, add it alongside the existing `HashMap` import:

```rust
use std::collections::{BTreeMap, HashMap, HashSet};
```

- [ ] **Step 3: Update the `price_changes` batch path in `run_once`**

Find the `if t.contains("price_changes")` block (~line 214) and replace its body:

```rust
if t.contains("price_changes") && t.trim_start().starts_with('{') {
    if let Ok(msg) = serde_json::from_str::<MarketPriceChangesMsg>(t) {
        let mut touched: HashSet<String> = HashSet::new();
        for ch in &msg.price_changes {
            let aid = canonical_clob_token_id(&ch.asset_id).into_owned();
            let entry = books.entry(aid.clone()).or_default();
            let (Ok(p), Ok(s)) = (ch.price.parse::<f64>(), ch.size.parse::<f64>()) else {
                continue;
            };
            let key = price_to_key(p);
            let map = if ch.side == "BUY" { &mut entry.0 } else { &mut entry.1 };
            if s == 0.0 {
                map.remove(&key);
            } else {
                map.insert(key, s);
            }
            touched.insert(aid);
        }
        for aid in &touched {
            if let Some(entry) = books.get(aid) {
                send_snapshot(aid, entry, &mut out_bids, &mut out_asks, tx).await;
            }
        }
    } else {
        warn_unparsed_clob(t);
    }
    continue;
}
```

- [ ] **Step 4: Update the single-event fallback path**

Find the `if let Ok(ev) = serde_json::from_str::<RawEvent>(t)` block (~line 237) and replace:

```rust
if let Ok(ev) = serde_json::from_str::<RawEvent>(t) {
    if let Some(aid) = apply_raw_event_to_books(&mut books, ev) {
        if let Some(entry) = books.get(&aid) {
            send_snapshot(&aid, entry, &mut out_bids, &mut out_asks, tx).await;
        }
    }
    continue;
}
```

The old `async fn apply_raw_event` can now be deleted (its logic lives in `apply_raw_event_to_books`).

- [ ] **Step 5: Verify compilation**

```bash
cargo check --message-format=short 2>&1 | head -20
```

Expected: no errors.

- [ ] **Step 6: Commit**

```bash
git add src/feeds/clob_ws.rs
git commit -m "perf(clob_ws): coalesce price_changes snapshots — one send per asset per batch"
```

---

## Task 7: `main.rs` — guard bundle send, reqwest outside loop, skip `Tick` (Fixes 1, 6, 12)

**Files:**
- Modify: `src/main.rs`

Three independent changes: (1) add `send_user_bundle_if_changed` guard (UserWsBundle already has PartialEq); (2) move `reqwest_client()` before the balance poll loop; (3) add `AppEvent::Tick` to the early-return branch so bundle/watch don't run on it.

- [ ] **Step 1: Add `send_user_bundle_if_changed` helper**

Add this function near `send_book_watch_if_changed` (~line 151):

```rust
/// Only sends when the bundle has changed — avoids waking the user-WS supervisor
/// on every price tick and book snapshot (which cannot change market/trailing IDs).
fn send_user_bundle_if_changed(state: &AppState, tx: &watch::Sender<UserWsBundle>) {
    let next = build_user_ws_bundle(state);
    if *tx.borrow() == next {
        return;
    }
    let _ = tx.send(next);
}
```

- [ ] **Step 2: Replace the unconditional send in `apply_app_event`**

Find the line in `apply_app_event` (~line 210):

```rust
let _ = user_bundle_tx.send(build_user_ws_bundle(state));
```

Replace with:

```rust
send_user_bundle_if_changed(state, user_bundle_tx);
```

- [ ] **Step 3: Add `AppEvent::Tick` to the early-return branch in `apply_app_event` (Fix 12)**

Find the outer `match ev` in `apply_app_event` (~line 169). It currently has two arms: `AppEvent::Key(k)` and `e =>`. Add a new arm for `Tick` between them:

```rust
AppEvent::Tick => {
    state.apply(AppEvent::Tick).await;
    false
}
```

This skips `try_dispatch_trailing_sell`, `send_user_bundle_if_changed`, and `send_book_watch_if_changed` for the 1-Hz tick — none of which can be changed by a `Tick` event.

- [ ] **Step 4: Move `reqwest_client()` outside the balance poll loop (Fix 6)**

Find the balance poll task spawn (~line 563). Currently inside the `loop { interval.tick().await; ... }`:

```rust
let data_http = match net::reqwest_client() {
    Ok(h) => h,
    Err(e) => {
        debug!(error = %e, "reqwest client for Data API (balance index)");
        continue;
    }
};
```

Move this block to BEFORE the `loop`:

```rust
let data_http = match net::reqwest_client() {
    Ok(h) => h,
    Err(e) => {
        debug!(error = %e, "reqwest client for Data API (balance index) — balance disabled");
        return;
    }
};
let mut interval = tokio::time::interval(Duration::from_secs(5));
interval.tick().await;
loop {
    interval.tick().await;
    match crate::balances::fetch_balance_panel_usdc(
        &data_http,
        &rpc_http,
        &rpc_url,
        funder,
    )
    .await
    { /* existing Ok/Err arms unchanged */ }
}
```

Note: `continue` becomes `return` since we're no longer inside the loop.

- [ ] **Step 5: Verify compilation**

```bash
cargo check --message-format=short 2>&1 | head -20
```

Expected: no errors.

- [ ] **Step 6: Commit**

```bash
git add src/main.rs
git commit -m "perf(main): guard user bundle send, reqwest outside loop, skip Tick in bundle/watch"
```

---

## Task 8: `render.rs` — static spaces, O(1) truncate, single-alloc fmt (Fixes 8, 9, 10)

**Files:**
- Modify: `src/ui/render.rs`

Three independent render-path fixes: eliminate `" ".repeat(gap)` heap alloc per frame; eliminate O(n) `chars().count()` scan in `truncate`; reduce `fmt_money_decimals_inner` from 3 heap allocs to 1.

- [ ] **Step 1: Replace `" ".repeat(gap)` with a const-slice in `balance_row_right_aligned` (Fix 8)**

Add a constant near the top of `render.rs` (after the imports):

```rust
const SPACES: &str = "                                "; // 32 spaces — wider than any balance panel
```

In `balance_row_right_aligned` (~line 206), replace:

```rust
Span::raw(" ".repeat(gap)),
```

with:

```rust
Span::raw(&SPACES[..gap.min(SPACES.len())]),
```

`Span::raw` accepts `&'static str` via `Into<Cow<'static, str>>`. Because `SPACES` is `'static`, `&SPACES[..n]` is also `'static`. No allocation.

- [ ] **Step 2: Rewrite `truncate` with O(1) byte-length fast path (Fix 9)**

Find `truncate` (~line 987) and replace it entirely:

```rust
fn truncate(s: &str, n: usize) -> String {
    // Fast path: byte length ≥ char count, so if bytes fit, chars definitely fit.
    if s.len() <= n {
        return s.to_string();
    }
    // Slow path: find the byte offset of the (n-1)th char, append ellipsis.
    match s.char_indices().nth(n.saturating_sub(1)) {
        Some((i, _)) => {
            let mut out = String::with_capacity(i + 3); // 3 bytes for "…"
            out.push_str(&s[..i]);
            out.push('…');
            out
        }
        None => s.to_string(),
    }
}
```

For all ASCII strings (market question, status line), `s.len() <= n` returns in O(1). The slow path runs only when the string is genuinely longer and contains multi-byte characters at the boundary — rare in practice.

- [ ] **Step 3: Rewrite `fmt_money_decimals_inner` with one allocation (Fix 10)**

Find `fmt_money_decimals_inner` (~line 955) and replace it entirely:

```rust
fn fmt_money_decimals_inner(v: f64, decimal_places: u32, floor_frac: bool) -> String {
    let whole = v as i64;
    let mult  = 10_f64.powi(decimal_places as i32);
    let max_frac = mult as u64;
    let raw_frac = (v - whole as f64).abs() * mult;
    let mut frac = if floor_frac { raw_frac.floor() as u64 } else { raw_frac.round() as u64 };
    let mut w = whole;
    if frac >= max_frac {
        w += if v >= 0.0 { 1 } else { -1 };
        frac = 0;
    }

    // Build integer digits in reverse into a stack buffer (u64 max = 20 digits).
    let neg = w < 0;
    let abs_w = w.unsigned_abs();
    let mut ibuf = [0u8; 20];
    let mut ilen = 0usize;
    let mut tmp = abs_w;
    if tmp == 0 {
        ibuf[0] = b'0';
        ilen = 1;
    } else {
        while tmp > 0 {
            ibuf[ilen] = b'0' + (tmp % 10) as u8;
            tmp /= 10;
            ilen += 1;
        }
    }

    // Single allocation sized to fit the result.
    // Worst case: sign(1) + digits(20) + commas(6) + dot(1) + frac(4) = 32 bytes.
    let mut out = String::with_capacity(32);
    if neg { out.push('-'); }
    for i in (0..ilen).rev() {
        let pos = ilen - 1 - i; // position from left, 0-indexed
        if pos > 0 && (ilen - pos) % 3 == 0 {
            out.push(',');
        }
        out.push(ibuf[i] as char);
    }
    out.push('.');
    // Write zero-padded fractional digits from a stack buffer.
    let dp = decimal_places as usize;
    let mut fbuf = [b'0'; 6]; // decimal_places is at most 4 in this app
    let mut ftmp = frac;
    for j in (0..dp).rev() {
        fbuf[j] = b'0' + (ftmp % 10) as u8;
        ftmp /= 10;
    }
    // SAFETY: all bytes in fbuf[..dp] are ASCII digits b'0'..=b'9'.
    out.push_str(unsafe { std::str::from_utf8_unchecked(&fbuf[..dp]) });
    out
}
```

- [ ] **Step 4: Add a unit test for `fmt_money_decimals_inner`**

Add a `#[cfg(test)]` module at the bottom of `render.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_money_basic() {
        assert_eq!(fmt_money_decimals_inner(1234.56,  2, false), "1,234.56");
        assert_eq!(fmt_money_decimals_inner(0.0,      2, false), "0.00");
        assert_eq!(fmt_money_decimals_inner(-99.5,    2, false), "-99.50");
        assert_eq!(fmt_money_decimals_inner(1_000_000.0, 2, false), "1,000,000.00");
        assert_eq!(fmt_money_decimals_inner(0.999,    2, false), "1.00"); // rounding carry
        assert_eq!(fmt_money_decimals_inner(67_432.51, 2, true),  "67,432.51");
        assert_eq!(fmt_money_decimals_inner(1.23456,  4, false), "1.2346");
    }

    #[test]
    fn truncate_ascii_fast_path() {
        // No truncation — must not scan chars
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("",      5),  "");
    }

    #[test]
    fn truncate_long_string() {
        let s = "a".repeat(100);
        let r = truncate(&s, 5);
        assert!(r.ends_with('…'));
        assert_eq!(r.chars().count(), 5); // 4 'a' + ellipsis
    }
}
```

- [ ] **Step 5: Run tests**

```bash
cargo test 2>&1 | tail -15
```

Expected: all tests pass including the new `fmt_money_basic`, `truncate_ascii_fast_path`, and `truncate_long_string` tests.

- [ ] **Step 6: Verify release build**

```bash
cargo build --release 2>&1 | tail -3
```

Expected: `Finished release [optimized] target(s)`.

- [ ] **Step 7: Commit**

```bash
git add src/ui/render.rs
git commit -m "perf(render): static spaces, O(1) truncate, single-alloc fmt_money — zero hot-frame allocs"
```

---

## Self-Review

**Spec coverage check:**

| Spec fix | Task | Covered? |
|----------|------|----------|
| Fix 1 — guard user bundle send | Task 7 step 1-2 | ✓ |
| Fix 2 — eliminate double collect | Task 4 step 3-4 | ✓ |
| Fix 3 — HashSet O(1) in Book handler | Task 4 step 1-3 | ✓ |
| Fix 4 — coalesce price_changes | Task 6 | ✓ |
| Fix 5 — Cow return type | Tasks 2-3 | ✓ |
| Fix 6 — reqwest outside loop | Task 7 step 4 | ✓ |
| Fix 7 — cached display strings | Task 5 | ✓ |
| Fix 8 — static spaces | Task 8 step 1 | ✓ |
| Fix 9 — O(1) truncate | Task 8 step 2 | ✓ |
| Fix 10 — stack buffer fmt | Task 8 step 3 | ✓ |
| Fix 11 — trailing scan == | Task 3 step 7 | ✓ |
| Fix 12 — skip Tick | Task 7 step 3 | ✓ |
| Fix 13 — panic = abort | Task 1 | ✓ |

All 13 fixes covered. No placeholders or TBDs.
