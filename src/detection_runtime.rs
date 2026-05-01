//! Detection indicator + window dataset — runs **inline** in [`crate::app::AppState`] or on a
//! Tokio worker fed by [`DetectionCmd`] (see `spawn_detection_worker` in `main.rs`).

use chrono::{DateTime, Utc};
use std::collections::VecDeque;

use crate::detection::{
    decide_window_bid, BidDecision, DetectionOutcome, ResolutionPrior, WindowBidConfig, WindowBidInput,
};
use crate::feeds::clob_ws::BookSnapshot;
use crate::gamma::ActiveMarket;

const DETECTION_WINDOW_BOOK_POINTS_MAX: usize = 512;
const DETECTION_WINDOW_SPOT_POINTS_MAX: usize = 4096;
const DETECTION_COMPLETED_WINDOWS_MAX: usize = 256;

/// Mid price from a CLOB book snapshot (best bid / best ask average).
#[inline]
fn book_mid(b: &BookSnapshot) -> Option<f64> {
    let bid = b.bids.first().map(|l| l.price);
    let ask = b.asks.first().map(|l| l.price);
    match (bid, ask) {
        (Some(b), Some(a)) => Some((b + a) / 2.0),
        (Some(b), None) => Some(b),
        (None, Some(a)) => Some(a),
        _ => None,
    }
}

/// Book/context bid decision for the active rolling market.
#[derive(Debug, Clone, Copy)]
pub struct DetectionSignalView {
    pub outcome: DetectionOutcome,
    pub decision: BidDecision,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct DetectionBookPoint {
    pub rel_secs: i64,
    pub secs_to_close: i64,
    pub bid: Option<f64>,
    pub ask: Option<f64>,
    pub mid: Option<f64>,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct DetectionSpotPoint {
    pub rel_secs: i64,
    pub secs_to_close: i64,
    pub price: f64,
    pub distance_bps: Option<f64>,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct DetectionWindowSample {
    pub asset_label: String,
    pub timeframe_label: String,
    pub market_slug: String,
    pub opens_at: DateTime<Utc>,
    pub closes_at: DateTime<Utc>,
    pub price_to_beat: Option<f64>,
    pub close_spot: Option<f64>,
    pub resolved: Option<DetectionOutcome>,
    pub spot_series: Vec<DetectionSpotPoint>,
    pub up_series: Vec<DetectionBookPoint>,
    pub down_series: Vec<DetectionBookPoint>,
}

/// Minimal market fields needed for detection (worker does not hold full [`ActiveMarket`]).
#[derive(Debug, Clone)]
pub struct DetectionMarketSnap {
    pub slug: String,
    pub opens_at: DateTime<Utc>,
    pub closes_at: DateTime<Utc>,
    pub tick_size: String,
}

impl From<&ActiveMarket> for DetectionMarketSnap {
    fn from(m: &ActiveMarket) -> Self {
        Self {
            slug: m.slug.clone(),
            opens_at: m.opens_at,
            closes_at: m.closes_at,
            tick_size: m.tick_size.clone(),
        }
    }
}

/// Commands for the async detection worker (same semantics as inline [`DetectionRuntime`]).
#[derive(Debug)]
pub enum DetectionCmd {
    Clear,
    MarketRolled {
        market: DetectionMarketSnap,
        asset_label: String,
        timeframe_label: String,
    },
    /// After main applied RTDS / Chainlink price + latch updates.
    SpotAndBeat {
        spot: f64,
        price_to_beat: Option<f64>,
    },
    SetPriceToBeat(Option<f64>),
    /// One-sided book update (`is_up` = UP outcome token).
    Book { is_up: bool, book: BookSnapshot },
}

pub struct DetectionRuntime {
    market: Option<DetectionMarketSnap>,
    spot_price: Option<f64>,
    price_to_beat: Option<f64>,
    book_up: Option<BookSnapshot>,
    book_down: Option<BookSnapshot>,
    prior: ResolutionPrior,
    current_window: Option<DetectionWindowSample>,
    completed: VecDeque<DetectionWindowSample>,
    last_signal: Option<DetectionSignalView>,
}

impl Default for DetectionRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl DetectionRuntime {
    pub fn new() -> Self {
        Self {
            market: None,
            spot_price: None,
            price_to_beat: None,
            book_up: None,
            book_down: None,
            prior: ResolutionPrior::default(),
            current_window: None,
            completed: VecDeque::with_capacity(DETECTION_COMPLETED_WINDOWS_MAX),
            last_signal: None,
        }
    }

    pub fn clear(&mut self) {
        *self = Self::new();
    }

    pub fn last_signal(&self) -> Option<DetectionSignalView> {
        self.last_signal
    }

    #[cfg(test)]
    pub fn resolution_prior_total_observations(&self) -> u32 {
        self.prior.total_observations()
    }

    #[cfg(test)]
    pub(crate) fn current_window(&self) -> Option<&DetectionWindowSample> {
        self.current_window.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn completed_windows(&self) -> &VecDeque<DetectionWindowSample> {
        &self.completed
    }

    pub fn apply(&mut self, cmd: DetectionCmd) {
        match cmd {
            DetectionCmd::Clear => self.clear(),
            DetectionCmd::MarketRolled {
                market,
                asset_label,
                timeframe_label,
            } => {
                self.finalize_detection_window_sample();
                self.market = Some(market);
                self.last_signal = None;
                self.start_detection_window_sample(asset_label, timeframe_label);
                self.recompute_detection_signal();
            }
            DetectionCmd::SpotAndBeat {
                spot,
                price_to_beat,
            } => {
                self.spot_price = Some(spot);
                self.price_to_beat = price_to_beat;
                self.update_detection_window_spot();
                self.recompute_detection_signal();
            }
            DetectionCmd::SetPriceToBeat(v) => {
                self.price_to_beat = v;
                self.update_detection_window_spot();
                self.recompute_detection_signal();
            }
            DetectionCmd::Book { is_up, book } => {
                if is_up {
                    self.book_up = Some(book.clone());
                } else {
                    self.book_down = Some(book.clone());
                }
                self.record_detection_book_point(is_up, &book);
                self.recompute_detection_signal();
            }
        }
    }

    fn start_detection_window_sample(&mut self, asset_label: String, timeframe_label: String) {
        let Some(market) = self.market.as_ref() else {
            self.current_window = None;
            return;
        };
        self.current_window = Some(DetectionWindowSample {
            asset_label,
            timeframe_label,
            market_slug: market.slug.clone(),
            opens_at: market.opens_at,
            closes_at: market.closes_at,
            price_to_beat: self.price_to_beat,
            close_spot: self.spot_price,
            resolved: None,
            spot_series: Vec::new(),
            up_series: Vec::new(),
            down_series: Vec::new(),
        });
    }

    fn update_detection_window_spot(&mut self) {
        let ptb = self.price_to_beat;
        if let Some(sample) = self.current_window.as_mut() {
            if sample.price_to_beat.is_none() {
                sample.price_to_beat = ptb;
            }
            sample.close_spot = self.spot_price;
            let Some(price) = self.spot_price else {
                return;
            };
            let now = Utc::now();
            let distance_bps = sample
                .price_to_beat
                .filter(|target| target.is_finite() && *target > 0.0)
                .map(|target| (price - target) / target * 10_000.0);
            if sample.spot_series.len() == DETECTION_WINDOW_SPOT_POINTS_MAX {
                sample.spot_series.remove(0);
            }
            sample.spot_series.push(DetectionSpotPoint {
                rel_secs: (now - sample.opens_at).num_seconds(),
                secs_to_close: (sample.closes_at - now).num_seconds(),
                price,
                distance_bps,
            });
        }
    }

    fn record_detection_book_point(&mut self, is_up: bool, book: &BookSnapshot) {
        let Some(sample) = self.current_window.as_mut() else {
            return;
        };
        let now = Utc::now();
        let point = DetectionBookPoint {
            rel_secs: (now - sample.opens_at).num_seconds(),
            secs_to_close: (sample.closes_at - now).num_seconds(),
            bid: book.bids.first().map(|l| l.price),
            ask: book.asks.first().map(|l| l.price),
            mid: book_mid(book),
        };
        let series = if is_up {
            &mut sample.up_series
        } else {
            &mut sample.down_series
        };
        if series.len() == DETECTION_WINDOW_BOOK_POINTS_MAX {
            series.remove(0);
        }
        series.push(point);
    }

    fn finalize_detection_window_sample(&mut self) {
        let Some(mut sample) = self.current_window.take() else {
            return;
        };
        if Utc::now() < sample.closes_at {
            self.current_window = Some(sample);
            return;
        }
        if sample.price_to_beat.is_none() {
            sample.price_to_beat = self.price_to_beat;
        }
        if sample.close_spot.is_none() {
            sample.close_spot = self.spot_price;
        }
        sample.resolved = match (sample.close_spot, sample.price_to_beat) {
            (Some(close), Some(target)) => Some(if close >= target {
                DetectionOutcome::Up
            } else {
                DetectionOutcome::Down
            }),
            _ => None,
        };
        self.learn_from_detection_window_sample(&sample);
        if self.completed.len() == DETECTION_COMPLETED_WINDOWS_MAX {
            self.completed.pop_front();
        }
        self.completed.push_back(sample);
    }

    fn learn_from_detection_window_sample(&mut self, sample: &DetectionWindowSample) {
        let Some(resolved) = sample.resolved else {
            return;
        };
        let window_secs = (sample.closes_at - sample.opens_at).num_seconds().max(1);
        for point in &sample.spot_series {
            let Some(distance_bps) = point.distance_bps else {
                continue;
            };
            self.prior.record_observation(
                distance_bps,
                point.rel_secs,
                window_secs,
                resolved,
            );
        }
    }

    fn recompute_detection_signal(&mut self) {
        let Some(market) = self.market.as_ref() else {
            self.last_signal = None;
            return;
        };
        let Some(spot_price) = self.spot_price else {
            self.last_signal = None;
            return;
        };
        let Some(price_to_beat) = self.price_to_beat else {
            self.last_signal = None;
            return;
        };
        let (Some(up_book), Some(down_book)) = (self.book_up.as_ref(), self.book_down.as_ref()) else {
            self.last_signal = None;
            return;
        };
        let window_secs = (market.closes_at - market.opens_at).num_seconds().max(1);
        let elapsed_secs = (Utc::now() - market.opens_at)
            .num_seconds()
            .clamp(0, window_secs);
        let cfg = WindowBidConfig {
            tick_size: market.tick_size.parse::<f64>().unwrap_or(0.01),
            ..WindowBidConfig::default()
        };
        let decision = decide_window_bid(
            &WindowBidInput {
                spot_price,
                price_to_beat,
                elapsed_secs,
                window_secs,
                up_book,
                down_book,
            },
            &cfg,
        );
        let outcome = match decision {
            BidDecision::Bid { outcome, .. }
            | BidDecision::Skip {
                best_outcome: Some(outcome),
                ..
            } => outcome,
            BidDecision::Skip {
                best_outcome: None, ..
            } => {
                if spot_price >= price_to_beat {
                    DetectionOutcome::Up
                } else {
                    DetectionOutcome::Down
                }
            }
        };
        self.last_signal = Some(DetectionSignalView { outcome, decision });
    }
}
