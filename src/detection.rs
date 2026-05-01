//! Maker-bid detector for short crypto up/down windows.
//!
//! The module is deliberately pure: feed it spot/target context plus both CLOB books, receive
//! either a skip reason or a concrete maker bid candidate.

use crate::feeds::clob_ws::BookSnapshot;

const CONTEXT_TIME_BUCKETS: usize = 5;
const CONTEXT_DISTANCE_BUCKETS: usize = 9;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetectionOutcome {
    Up,
    Down,
}

impl DetectionOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Up => "UP",
            Self::Down => "DOWN",
        }
    }
}

#[cfg(test)]
#[derive(Debug, Clone, Copy)]
struct DetectionContext {
    outcome: DetectionOutcome,
    spot_price: f64,
    price_to_beat: f64,
    elapsed_secs: i64,
    window_secs: i64,
}

#[derive(Debug, Clone, Copy)]
pub struct WindowBidConfig {
    pub tick_size: f64,
    pub min_edge: f64,
    pub min_seconds_to_close: i64,
    pub max_spread_ticks: u32,
    pub min_top_bid_size: f64,
    pub min_depth_imbalance: f64,
}

impl Default for WindowBidConfig {
    fn default() -> Self {
        Self {
            tick_size: 0.01,
            min_edge: 0.03,
            min_seconds_to_close: 8,
            max_spread_ticks: 4,
            min_top_bid_size: 0.0,
            min_depth_imbalance: -0.60,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct WindowBidInput<'a> {
    pub spot_price: f64,
    pub price_to_beat: f64,
    pub elapsed_secs: i64,
    pub window_secs: i64,
    pub up_book: &'a BookSnapshot,
    pub down_book: &'a BookSnapshot,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BidDecision {
    Skip {
        reason: BidSkipReason,
        best_outcome: Option<DetectionOutcome>,
        best_fair_probability: Option<f64>,
    },
    Bid {
        outcome: DetectionOutcome,
        bid_price: f64,
        fair_probability: f64,
        expected_value: f64,
        edge: f64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BidSkipReason {
    BadInput,
    TooLate,
    NoBook,
    SpreadTooWide,
    QueueTooThin,
    BookImbalance,
    NoPositiveEdge,
}

#[derive(Debug, Clone, Copy, Default)]
struct ResolutionCounts {
    up: u32,
    down: u32,
}

#[derive(Debug, Clone, Default)]
pub struct ResolutionPrior {
    buckets: [[ResolutionCounts; CONTEXT_DISTANCE_BUCKETS]; CONTEXT_TIME_BUCKETS],
}

impl ResolutionPrior {
    pub fn record_observation(
        &mut self,
        distance_bps: f64,
        elapsed_secs: i64,
        window_secs: i64,
        resolved: DetectionOutcome,
    ) {
        let Some((time, distance)) = context_bucket(distance_bps, elapsed_secs, window_secs) else {
            return;
        };
        let counts = &mut self.buckets[time][distance];
        match resolved {
            DetectionOutcome::Up => counts.up = counts.up.saturating_add(1),
            DetectionOutcome::Down => counts.down = counts.down.saturating_add(1),
        }
    }

    #[cfg(test)]
    pub fn total_observations(&self) -> u32 {
        self.buckets
            .iter()
            .flatten()
            .map(|counts| counts.up.saturating_add(counts.down))
            .sum()
    }

    fn estimate(
        &self,
        outcome: DetectionOutcome,
        distance_bps: f64,
        elapsed_secs: i64,
        window_secs: i64,
    ) -> Option<ResolutionPriorEstimate> {
        let (time, distance) = context_bucket(distance_bps, elapsed_secs, window_secs)?;
        let counts = self.buckets[time][distance];
        let total = counts.up.saturating_add(counts.down);
        if total == 0 {
            return None;
        }
        let up_probability = (counts.up as f64 + 1.0) / (total as f64 + 2.0);
        let probability = match outcome {
            DetectionOutcome::Up => up_probability,
            DetectionOutcome::Down => 1.0 - up_probability,
        };
        let confidence = (total as f64 / 24.0).clamp(0.0, 0.70);
        Some(ResolutionPriorEstimate {
            probability,
            confidence,
        })
    }
}

#[derive(Debug, Clone, Copy)]
struct ResolutionPriorEstimate {
    probability: f64,
    confidence: f64,
}

pub fn decide_window_bid(input: &WindowBidInput<'_>, cfg: &WindowBidConfig) -> BidDecision {
    if !input.spot_price.is_finite()
        || !input.price_to_beat.is_finite()
        || input.spot_price <= 0.0
        || input.price_to_beat <= 0.0
        || input.window_secs <= 0
        || cfg.tick_size <= 0.0
        || !cfg.tick_size.is_finite()
    {
        return BidDecision::Skip {
            reason: BidSkipReason::BadInput,
            best_outcome: None,
            best_fair_probability: None,
        };
    }

    let remaining_secs = input
        .window_secs
        .saturating_sub(input.elapsed_secs.clamp(0, input.window_secs));
    if remaining_secs < cfg.min_seconds_to_close {
        return BidDecision::Skip {
            reason: BidSkipReason::TooLate,
            best_outcome: None,
            best_fair_probability: None,
        };
    }

    // Shared across UP/DOWN: same oracle context, only the favorable-bps sign flips per outcome.
    let (raw_bps, scale_bps) = match raw_and_scale_bps(
        input.spot_price,
        input.price_to_beat,
        input.elapsed_secs,
        input.window_secs,
    ) {
        Some(v) => v,
        None => {
            return BidDecision::Skip {
                reason: BidSkipReason::BadInput,
                best_outcome: None,
                best_fair_probability: None,
            };
        }
    };

    let up = bid_candidate(
        DetectionOutcome::Up,
        input.up_book,
        raw_bps,
        scale_bps,
        input.elapsed_secs,
        input.window_secs,
        cfg,
        None,
    );
    let down = bid_candidate(
        DetectionOutcome::Down,
        input.down_book,
        raw_bps,
        scale_bps,
        input.elapsed_secs,
        input.window_secs,
        cfg,
        None,
    );

    let best_fair = [up.as_ref(), down.as_ref()]
        .into_iter()
        .flatten()
        .max_by(|a, b| a.fair_probability.total_cmp(&b.fair_probability))
        .map(|c| (c.outcome, c.fair_probability));

    [up, down]
        .into_iter()
        .flatten()
        .max_by(|a, b| a.expected_value.total_cmp(&b.expected_value))
        .map(|c| BidDecision::Bid {
            outcome: c.outcome,
            bid_price: c.bid_price,
            fair_probability: c.fair_probability,
            expected_value: c.expected_value,
            edge: c.edge,
        })
        .unwrap_or_else(|| BidDecision::Skip {
            reason: bid_skip_reason(input, cfg),
            best_outcome: best_fair.map(|(outcome, _)| outcome),
            best_fair_probability: best_fair.map(|(_, probability)| probability),
        })
}

/// Oracle distance (bps vs target) and heuristic temperature — reused for both outcomes.
#[inline]
fn raw_and_scale_bps(
    spot_price: f64,
    price_to_beat: f64,
    elapsed_secs: i64,
    window_secs: i64,
) -> Option<(f64, f64)> {
    if !spot_price.is_finite()
        || !price_to_beat.is_finite()
        || spot_price <= 0.0
        || price_to_beat <= 0.0
        || window_secs <= 0
    {
        return None;
    }
    let raw_bps = (spot_price - price_to_beat) / price_to_beat * 10_000.0;
    if !raw_bps.is_finite() {
        return None;
    }
    let elapsed = elapsed_secs.clamp(0, window_secs) as f64;
    let remaining_frac = 1.0 - elapsed / window_secs as f64;
    let scale_bps = 12.0 + 90.0 * remaining_frac;
    Some((raw_bps, scale_bps))
}

#[derive(Debug, Clone, Copy)]
struct WindowBidCandidate {
    outcome: DetectionOutcome,
    bid_price: f64,
    fair_probability: f64,
    expected_value: f64,
    edge: f64,
}

fn bid_candidate(
    outcome: DetectionOutcome,
    book: &BookSnapshot,
    raw_bps: f64,
    scale_bps: f64,
    elapsed_secs: i64,
    window_secs: i64,
    cfg: &WindowBidConfig,
    resolution_prior: Option<&ResolutionPrior>,
) -> Option<WindowBidCandidate> {
    let bid0 = book.bids.first()?;
    let ask0 = book.asks.first()?;
    let best_bid = bid0.price;
    let best_ask = ask0.price;
    if !is_probability(best_bid) || !is_probability(best_ask) || best_bid >= best_ask {
        return None;
    }

    let spread_ticks = ((best_ask - best_bid) / cfg.tick_size).round();
    if spread_ticks > cfg.max_spread_ticks as f64 {
        return None;
    }

    let top_bid_size = bid0.size;
    if top_bid_size < cfg.min_top_bid_size {
        return None;
    }

    let imbalance = depth_imbalance(book, 3);
    if imbalance < cfg.min_depth_imbalance {
        return None;
    }

    let fair_probability = fair_probability_for_outcome(
        outcome,
        raw_bps,
        scale_bps,
        elapsed_secs,
        window_secs,
        resolution_prior,
    )?;
    let bid_price = maker_bid_price(best_bid, best_ask, fair_probability, cfg)?;
    let edge = fair_probability - bid_price;
    let expected_value = binary_expected_value(fair_probability, bid_price);
    (edge >= cfg.min_edge && expected_value > 0.0).then_some(WindowBidCandidate {
        outcome,
        bid_price,
        fair_probability,
        expected_value,
        edge,
    })
}

fn maker_bid_price(
    best_bid: f64,
    best_ask: f64,
    fair_probability: f64,
    cfg: &WindowBidConfig,
) -> Option<f64> {
    let improved = snap_down(best_bid + cfg.tick_size, cfg.tick_size);
    let last_maker_bid = snap_down(best_ask - cfg.tick_size, cfg.tick_size);
    let max_edge_bid = snap_down(fair_probability - cfg.min_edge, cfg.tick_size);
    let bid = improved.min(last_maker_bid).min(max_edge_bid);
    (is_probability(bid) && bid >= cfg.tick_size && bid > best_bid && bid < best_ask).then_some(bid)
}

fn bid_skip_reason(input: &WindowBidInput<'_>, cfg: &WindowBidConfig) -> BidSkipReason {
    let up = input.up_book;
    let down = input.down_book;
    if up.bids.first().is_none()
        || up.asks.first().is_none()
        || down.bids.first().is_none()
        || down.asks.first().is_none()
    {
        return BidSkipReason::NoBook;
    }
    let spread_wide = |book: &BookSnapshot| {
        let bid = book.bids.first().map(|level| level.price).unwrap_or(0.0);
        let ask = book.asks.first().map(|level| level.price).unwrap_or(1.0);
        ((ask - bid) / cfg.tick_size).round() > cfg.max_spread_ticks as f64
    };
    if spread_wide(up) && spread_wide(down) {
        return BidSkipReason::SpreadTooWide;
    }
    let thin = |book: &BookSnapshot| {
        book.bids
            .first()
            .map(|level| level.size < cfg.min_top_bid_size)
            .unwrap_or(true)
    };
    if thin(up) && thin(down) {
        return BidSkipReason::QueueTooThin;
    }
    let imb_up = depth_imbalance(up, 3);
    let imb_down = depth_imbalance(down, 3);
    if imb_up < cfg.min_depth_imbalance && imb_down < cfg.min_depth_imbalance {
        return BidSkipReason::BookImbalance;
    }
    BidSkipReason::NoPositiveEdge
}

fn binary_expected_value(win_probability: f64, price: f64) -> f64 {
    win_probability * (1.0 - price) - (1.0 - win_probability) * price
}

/// Fair probability for one outcome; `raw_bps` / `scale_bps` from [`raw_and_scale_bps`].
#[inline]
fn fair_probability_for_outcome(
    outcome: DetectionOutcome,
    raw_bps: f64,
    scale_bps: f64,
    elapsed_secs: i64,
    window_secs: i64,
    resolution_prior: Option<&ResolutionPrior>,
) -> Option<f64> {
    if !scale_bps.is_finite() || scale_bps <= 0.0 {
        return None;
    }
    let favorable_bps = match outcome {
        DetectionOutcome::Up => raw_bps,
        DetectionOutcome::Down => -raw_bps,
    };
    let heuristic = sigmoid(favorable_bps / scale_bps).clamp(0.01, 0.99);
    if let Some(learned) = resolution_prior
        .and_then(|prior| prior.estimate(outcome, raw_bps, elapsed_secs, window_secs))
    {
        Some(
            (heuristic * (1.0 - learned.confidence) + learned.probability * learned.confidence)
                .clamp(0.01, 0.99),
        )
    } else {
        Some(heuristic)
    }
}

#[cfg(test)]
fn contextual_resolution_probability(
    ctx: &DetectionContext,
    resolution_prior: Option<&ResolutionPrior>,
) -> Option<f64> {
    let (raw_bps, scale_bps) = raw_and_scale_bps(
        ctx.spot_price,
        ctx.price_to_beat,
        ctx.elapsed_secs,
        ctx.window_secs,
    )?;
    fair_probability_for_outcome(
        ctx.outcome,
        raw_bps,
        scale_bps,
        ctx.elapsed_secs,
        ctx.window_secs,
        resolution_prior,
    )
}

fn context_bucket(
    distance_bps: f64,
    elapsed_secs: i64,
    window_secs: i64,
) -> Option<(usize, usize)> {
    if !distance_bps.is_finite() || window_secs <= 0 {
        return None;
    }
    let elapsed = elapsed_secs.clamp(0, window_secs) as f64;
    let progress = elapsed / window_secs as f64;
    let time =
        ((progress * CONTEXT_TIME_BUCKETS as f64).floor() as usize).min(CONTEXT_TIME_BUCKETS - 1);
    let clipped = distance_bps.clamp(-200.0, 200.0);
    let distance = (((clipped + 200.0) / 400.0) * CONTEXT_DISTANCE_BUCKETS as f64).floor() as usize;
    Some((time, distance.min(CONTEXT_DISTANCE_BUCKETS - 1)))
}

fn sigmoid(x: f64) -> f64 {
    1.0 / (1.0 + (-x).exp())
}

fn depth_imbalance(book: &BookSnapshot, levels: usize) -> f64 {
    debug_assert!(levels <= 8);
    let mut bid_depth = 0.0_f64;
    for level in book.bids.iter().take(levels) {
        let s = level.size;
        if s.is_finite() && s > 0.0 {
            bid_depth += s;
        }
    }
    let mut ask_depth = 0.0_f64;
    for level in book.asks.iter().take(levels) {
        let s = level.size;
        if s.is_finite() && s > 0.0 {
            ask_depth += s;
        }
    }
    let total = bid_depth + ask_depth;
    if total <= 0.0 {
        0.0
    } else {
        (bid_depth - ask_depth) / total
    }
}

fn snap_down(price: f64, tick_size: f64) -> f64 {
    if !price.is_finite() || !tick_size.is_finite() || tick_size <= 0.0 {
        return price;
    }
    ((price / tick_size).floor() * tick_size * 100.0).round() / 100.0
}

fn is_probability(v: f64) -> bool {
    v.is_finite() && (0.0..=1.0).contains(&v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feeds::clob_ws::{BookLevel, BookSnapshot};

    fn book(bid: f64, ask: f64) -> BookSnapshot {
        BookSnapshot {
            asset_id: "asset".to_string(),
            bids: vec![
                BookLevel {
                    price: bid,
                    size: 100.0,
                },
                BookLevel {
                    price: bid - 0.01,
                    size: 50.0,
                },
            ],
            asks: vec![
                BookLevel {
                    price: ask,
                    size: 80.0,
                },
                BookLevel {
                    price: ask + 0.01,
                    size: 40.0,
                },
            ],
        }
    }

    fn bid_cfg() -> WindowBidConfig {
        WindowBidConfig {
            tick_size: 0.01,
            min_edge: 0.03,
            min_seconds_to_close: 8,
            max_spread_ticks: 6,
            min_top_bid_size: 1.0,
            min_depth_imbalance: -0.80,
        }
    }

    #[test]
    fn resolution_prior_learns_context_from_completed_windows() {
        let ctx = DetectionContext {
            outcome: DetectionOutcome::Up,
            spot_price: 100.05,
            price_to_beat: 100.0,
            elapsed_secs: 240,
            window_secs: 300,
        };
        let heuristic = contextual_resolution_probability(&ctx, None).unwrap();
        let mut prior = ResolutionPrior::default();
        for _ in 0..48 {
            prior.record_observation(5.0, 240, 300, DetectionOutcome::Down);
        }
        let learned = contextual_resolution_probability(&ctx, Some(&prior)).unwrap();

        assert!(
            learned < heuristic,
            "learned prior should pull weak above-target contexts down; heuristic={heuristic}, learned={learned}"
        );
        assert_eq!(prior.total_observations(), 48);
    }

    #[test]
    fn window_bid_decision_bids_up_one_tick_when_edge_survives() {
        let up = book(0.56, 0.60);
        let down = book(0.36, 0.40);
        let decision = decide_window_bid(
            &WindowBidInput {
                spot_price: 101.0,
                price_to_beat: 100.0,
                elapsed_secs: 240,
                window_secs: 300,
                up_book: &up,
                down_book: &down,
            },
            &bid_cfg(),
        );

        match decision {
            BidDecision::Bid {
                outcome,
                bid_price,
                edge,
                ..
            } => {
                assert_eq!(outcome, DetectionOutcome::Up);
                assert_eq!(bid_price, 0.57);
                assert!(edge >= bid_cfg().min_edge);
            }
            other => panic!("expected bid, got {other:?}"),
        }
    }

    #[test]
    fn window_bid_decision_bids_down_when_spot_is_below_target() {
        let up = book(0.36, 0.40);
        let down = book(0.55, 0.59);
        let decision = decide_window_bid(
            &WindowBidInput {
                spot_price: 99.0,
                price_to_beat: 100.0,
                elapsed_secs: 240,
                window_secs: 300,
                up_book: &up,
                down_book: &down,
            },
            &bid_cfg(),
        );

        match decision {
            BidDecision::Bid {
                outcome, bid_price, ..
            } => {
                assert_eq!(outcome, DetectionOutcome::Down);
                assert_eq!(bid_price, 0.56);
            }
            other => panic!("expected bid, got {other:?}"),
        }
    }

    #[test]
    fn window_bid_decision_skips_when_too_close_to_close() {
        let up = book(0.50, 0.54);
        let down = book(0.44, 0.48);
        let decision = decide_window_bid(
            &WindowBidInput {
                spot_price: 101.0,
                price_to_beat: 100.0,
                elapsed_secs: 296,
                window_secs: 300,
                up_book: &up,
                down_book: &down,
            },
            &bid_cfg(),
        );

        assert_eq!(
            decision,
            BidDecision::Skip {
                reason: BidSkipReason::TooLate,
                best_outcome: None,
                best_fair_probability: None,
            }
        );
    }

    #[test]
    fn window_bid_decision_does_not_cross_tight_book() {
        let up = book(0.58, 0.59);
        let down = book(0.40, 0.41);
        let decision = decide_window_bid(
            &WindowBidInput {
                spot_price: 101.0,
                price_to_beat: 100.0,
                elapsed_secs: 240,
                window_secs: 300,
                up_book: &up,
                down_book: &down,
            },
            &bid_cfg(),
        );

        assert!(matches!(
            decision,
            BidDecision::Skip {
                reason: BidSkipReason::NoPositiveEdge,
                ..
            }
        ));
    }
}
