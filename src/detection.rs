//! Mispricing detector for maker-bid candidates.
//!
//! The module is deliberately pure: feed it a price path and the current market price,
//! receive a signal that can be rendered or later wired into order placement.

const BUCKETS: usize = 10;
const RESOLUTION_THRESHOLD: f64 = 0.50;
const CONTEXT_TIME_BUCKETS: usize = 5;
const CONTEXT_DISTANCE_BUCKETS: usize = 9;

#[derive(Debug, Clone, Copy)]
pub struct DetectionConfig {
    pub simulations: usize,
    pub horizon_steps: usize,
    pub min_delta: f64,
    pub max_kelly_fraction: f64,
    pub longshot_yes_ceiling: f64,
    pub near_certain_floor: f64,
    pub near_certain_bonus: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetectionOutcome {
    Up,
    Down,
}

#[derive(Debug, Clone, Copy)]
pub struct DetectionContext {
    pub outcome: DetectionOutcome,
    pub spot_price: f64,
    pub price_to_beat: f64,
    pub elapsed_secs: i64,
    pub window_secs: i64,
}

#[derive(Debug, Clone, Default)]
pub struct MarkovPrior {
    counts: [[u32; BUCKETS]; BUCKETS],
}

impl MarkovPrior {
    pub fn record_transition(&mut self, from: f64, to: f64) {
        let (Some(from), Some(to)) = (bucket(from), bucket(to)) else {
            return;
        };
        self.counts[from][to] = self.counts[from][to].saturating_add(1);
    }

    pub fn total_transitions(&self) -> u32 {
        self.counts.iter().flatten().copied().sum()
    }

    fn is_empty(&self) -> bool {
        self.total_transitions() == 0
    }
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
        // Laplace smoothing keeps a single early observation from forcing 0%/100%.
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

#[derive(Debug, Clone)]
pub struct DetectionCandidate {
    pub outcome: DetectionOutcome,
    pub prices: Vec<f64>,
    pub market_probability: f64,
    pub context: Option<DetectionContext>,
    pub prior: MarkovPrior,
    pub resolution_prior: ResolutionPrior,
}

#[derive(Debug, Clone)]
pub struct DetectionJob {
    pub seq: u64,
    pub market_slug: Option<String>,
    pub candidates: Vec<DetectionCandidate>,
    pub cfg: DetectionConfig,
}

#[derive(Debug, Clone)]
pub struct DetectionJobResult {
    pub seq: u64,
    pub market_slug: Option<String>,
    pub best: Option<(DetectionOutcome, BidSignal)>,
}

impl Default for DetectionConfig {
    fn default() -> Self {
        Self {
            simulations: 10_000,
            horizon_steps: 12,
            min_delta: 0.03,
            max_kelly_fraction: 0.20,
            longshot_yes_ceiling: 0.30,
            near_certain_floor: 0.80,
            near_certain_bonus: 0.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BidRecommendation {
    Skip,
    MakerUp,
    MakerDown,
}

#[derive(Debug, Clone, Copy)]
pub struct BidSignal {
    pub recommendation: BidRecommendation,
    pub fair_probability: f64,
    pub markov_probability: f64,
    pub context_probability: f64,
    pub market_probability: f64,
    pub delta: f64,
    pub expected_value: f64,
    pub kelly_fraction: f64,
    pub no_delta: f64,
    pub no_expected_value: f64,
    pub no_kelly_fraction: f64,
}

#[derive(Debug, Clone)]
struct TransitionMatrix {
    rows: [[f64; BUCKETS]; BUCKETS],
}

/// Build a signal from a historical YES-price path and the current YES market price.
#[cfg(test)]
pub fn detect_bid_signal(
    yes_prices: &[f64],
    market_probability: f64,
    cfg: &DetectionConfig,
) -> Option<BidSignal> {
    detect_bid_signal_inner(yes_prices, market_probability, None, None, None, cfg)
}

#[cfg(test)]
pub fn detect_bid_signal_with_context(
    yes_prices: &[f64],
    market_probability: f64,
    context: &DetectionContext,
    cfg: &DetectionConfig,
) -> Option<BidSignal> {
    detect_bid_signal_inner(yes_prices, market_probability, Some(context), None, None, cfg)
}

#[cfg(test)]
pub fn detect_bid_signal_with_prior(
    yes_prices: &[f64],
    market_probability: f64,
    prior: &MarkovPrior,
    cfg: &DetectionConfig,
) -> Option<BidSignal> {
    detect_bid_signal_inner(yes_prices, market_probability, None, Some(prior), None, cfg)
}

pub fn run_detection_job(job: DetectionJob) -> DetectionJobResult {
    let best = job
        .candidates
        .iter()
        .filter_map(|candidate| {
            let signal = detect_bid_signal_inner(
                &candidate.prices,
                candidate.market_probability,
                candidate.context.as_ref(),
                Some(&candidate.prior),
                Some(&candidate.resolution_prior),
                &job.cfg,
            )?;
            Some((candidate.outcome, signal))
        })
        .max_by(|(_, a), (_, b)| signal_score(*a).total_cmp(&signal_score(*b)));

    DetectionJobResult {
        seq: job.seq,
        market_slug: job.market_slug,
        best,
    }
}

fn detect_bid_signal_inner(
    yes_prices: &[f64],
    market_probability: f64,
    context: Option<&DetectionContext>,
    prior: Option<&MarkovPrior>,
    resolution_prior: Option<&ResolutionPrior>,
    cfg: &DetectionConfig,
) -> Option<BidSignal> {
    if yes_prices.len() < 2
        || !is_probability(market_probability)
        || cfg.simulations == 0
        || cfg.horizon_steps == 0
    {
        return None;
    }

    let matrix = TransitionMatrix::from_prices_and_prior(yes_prices, prior)?;
    let current = *yes_prices.last()?;
    let markov_probability = markov_yes_probability(&matrix, current, cfg);
    let context_probability = context
        .and_then(|ctx| contextual_resolution_probability(ctx, resolution_prior))
        .unwrap_or(markov_probability);
    let context_weight = context.map(context_weight).unwrap_or(0.0);
    let mut fair_probability =
        markov_probability * (1.0 - context_weight) + context_probability * context_weight;
    if market_probability >= cfg.near_certain_floor && fair_probability > market_probability {
        fair_probability = (fair_probability + cfg.near_certain_bonus).min(0.99);
    }

    let delta = fair_probability - market_probability;
    let expected_value = binary_expected_value(fair_probability, market_probability);
    let yes_kelly_fraction =
        kelly_fraction(fair_probability, market_probability, cfg.max_kelly_fraction);

    let no_market_probability = 1.0 - market_probability;
    let no_fair_probability = 1.0 - fair_probability;
    let no_delta = no_fair_probability - no_market_probability;
    let no_expected_value = binary_expected_value(no_fair_probability, no_market_probability);
    let no_kelly_fraction = kelly_fraction(
        no_fair_probability,
        no_market_probability,
        cfg.max_kelly_fraction,
    );

    let recommendation =
        if delta > cfg.min_delta && expected_value > 0.0 && yes_kelly_fraction > 0.0 {
            BidRecommendation::MakerUp
        } else if market_probability <= cfg.longshot_yes_ceiling
            && no_delta > cfg.min_delta
            && no_expected_value > 0.0
            && no_kelly_fraction > 0.0
        {
            BidRecommendation::MakerDown
        } else {
            BidRecommendation::Skip
        };

    Some(BidSignal {
        recommendation,
        fair_probability,
        markov_probability,
        context_probability,
        market_probability,
        delta,
        expected_value,
        kelly_fraction: yes_kelly_fraction,
        no_delta,
        no_expected_value,
        no_kelly_fraction,
    })
}

impl BidSignal {
    pub fn short_label(self) -> String {
        match self.recommendation {
            BidRecommendation::Skip => format!(
                "fair {:.0}% (M {:.0}%/ctx {:.0}%) EDGE {:+.1}% EV {:+.3} Kelly {:.0}% -> skip",
                self.fair_probability * 100.0,
                self.markov_probability * 100.0,
                self.context_probability * 100.0,
                self.delta * 100.0,
                self.expected_value,
                self.kelly_fraction * 100.0
            ),
            BidRecommendation::MakerUp => format!(
                "fair {:.0}% (M {:.0}%/ctx {:.0}%) EDGE {:+.1}% EV {:+.3} Kelly {:.0}% -> BID UP @ {:.2}",
                self.fair_probability * 100.0,
                self.markov_probability * 100.0,
                self.context_probability * 100.0,
                self.delta * 100.0,
                self.expected_value,
                self.kelly_fraction * 100.0,
                self.market_probability
            ),
            BidRecommendation::MakerDown => format!(
                "NO edge {:+.1}% EV {:+.3} Kelly {:.0}% -> BID DOWN @ {:.2}",
                self.no_delta * 100.0,
                self.no_expected_value,
                self.no_kelly_fraction * 100.0,
                1.0 - self.market_probability
            ),
        }
    }
}

impl TransitionMatrix {
    fn from_prices_and_prior(prices: &[f64], prior: Option<&MarkovPrior>) -> Option<Self> {
        let mut counts = [[0_u32; BUCKETS]; BUCKETS];
        if let Some(prior) = prior.filter(|prior| !prior.is_empty()) {
            counts = prior.counts;
        }
        for pair in prices.windows(2) {
            let from = bucket(pair[0])?;
            let to = bucket(pair[1])?;
            counts[from][to] = counts[from][to].saturating_add(1);
        }

        let mut rows = [[0.0; BUCKETS]; BUCKETS];
        for from in 0..BUCKETS {
            let total: u32 = counts[from].iter().sum();
            if total == 0 {
                rows[from][from] = 1.0;
                continue;
            }
            for to in 0..BUCKETS {
                rows[from][to] = counts[from][to] as f64 / total as f64;
            }
        }
        Some(Self { rows })
    }

}

fn markov_yes_probability(matrix: &TransitionMatrix, current: f64, cfg: &DetectionConfig) -> f64 {
    let Some(start) = bucket(current) else {
        return 0.0;
    };
    let mut dist = [0.0_f64; BUCKETS];
    dist[start] = 1.0;

    for _ in 0..cfg.horizon_steps {
        let mut next = [0.0_f64; BUCKETS];
        for from in 0..BUCKETS {
            let mass = dist[from];
            if mass <= 0.0 {
                continue;
            }
            for to in 0..BUCKETS {
                next[to] += mass * matrix.rows[from][to];
            }
        }
        dist = next;
    }

    dist.iter()
        .enumerate()
        .filter(|(bucket, _)| bucket_midpoint(*bucket) > RESOLUTION_THRESHOLD)
        .map(|(_, probability)| probability)
        .sum::<f64>()
        .clamp(0.0, 1.0)
}

fn binary_expected_value(win_probability: f64, price: f64) -> f64 {
    win_probability * (1.0 - price) - (1.0 - win_probability) * price
}

fn signal_score(signal: BidSignal) -> f64 {
    match signal.recommendation {
        BidRecommendation::MakerUp => signal.expected_value,
        BidRecommendation::MakerDown => signal.no_expected_value,
        BidRecommendation::Skip => signal.expected_value.max(signal.no_expected_value),
    }
}

fn contextual_resolution_probability(
    ctx: &DetectionContext,
    resolution_prior: Option<&ResolutionPrior>,
) -> Option<f64> {
    if !ctx.spot_price.is_finite()
        || !ctx.price_to_beat.is_finite()
        || ctx.spot_price <= 0.0
        || ctx.price_to_beat <= 0.0
        || ctx.window_secs <= 0
    {
        return None;
    }
    let raw_bps = (ctx.spot_price - ctx.price_to_beat) / ctx.price_to_beat * 10_000.0;
    let favorable_bps = match ctx.outcome {
        DetectionOutcome::Up => raw_bps,
        DetectionOutcome::Down => -raw_bps,
    };
    let elapsed = ctx.elapsed_secs.clamp(0, ctx.window_secs) as f64;
    let remaining_frac = 1.0 - elapsed / ctx.window_secs as f64;
    // Early in the window spot can mean-revert; near close the same distance is much stronger.
    let scale_bps = 12.0 + 90.0 * remaining_frac;
    let heuristic = sigmoid(favorable_bps / scale_bps).clamp(0.01, 0.99);
    if let Some(learned) = resolution_prior.and_then(|prior| {
        prior.estimate(ctx.outcome, raw_bps, ctx.elapsed_secs, ctx.window_secs)
    }) {
        Some(
            (heuristic * (1.0 - learned.confidence) + learned.probability * learned.confidence)
                .clamp(0.01, 0.99),
        )
    } else {
        Some(heuristic)
    }
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
    let time = ((progress * CONTEXT_TIME_BUCKETS as f64).floor() as usize)
        .min(CONTEXT_TIME_BUCKETS - 1);
    let clipped = distance_bps.clamp(-200.0, 200.0);
    let distance = (((clipped + 200.0) / 400.0) * CONTEXT_DISTANCE_BUCKETS as f64).floor()
        as usize;
    Some((time, distance.min(CONTEXT_DISTANCE_BUCKETS - 1)))
}

fn context_weight(ctx: &DetectionContext) -> f64 {
    if ctx.window_secs <= 0 {
        return 0.0;
    }
    let elapsed = ctx.elapsed_secs.clamp(0, ctx.window_secs) as f64;
    let progress = elapsed / ctx.window_secs as f64;
    (0.35 + 0.45 * progress).clamp(0.0, 0.80)
}

fn sigmoid(x: f64) -> f64 {
    1.0 / (1.0 + (-x).exp())
}

fn kelly_fraction(win_probability: f64, price: f64, cap: f64) -> f64 {
    if !is_probability(price) || price >= 1.0 {
        return 0.0;
    }
    let payoff_ratio = (1.0 - price) / price;
    if payoff_ratio <= 0.0 {
        return 0.0;
    }
    let q = 1.0 - win_probability;
    ((win_probability * payoff_ratio - q) / payoff_ratio).clamp(0.0, cap.max(0.0))
}

fn bucket(price: f64) -> Option<usize> {
    if !is_probability(price) {
        return None;
    }
    let idx = (price * BUCKETS as f64).floor() as usize;
    Some(idx.min(BUCKETS - 1))
}

fn bucket_midpoint(bucket: usize) -> f64 {
    (bucket as f64 + 0.5) / BUCKETS as f64
}

fn is_probability(v: f64) -> bool {
    v.is_finite() && (0.0..=1.0).contains(&v)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> DetectionConfig {
        DetectionConfig {
            simulations: 2_000,
            horizon_steps: 8,
            min_delta: 0.02,
            max_kelly_fraction: 0.25,
            longshot_yes_ceiling: 0.30,
            near_certain_floor: 0.80,
            near_certain_bonus: 0.0,
        }
    }

    #[test]
    fn learns_upward_transition_edge_and_recommends_maker_yes_bid() {
        let prices = [
            0.62, 0.66, 0.71, 0.74, 0.79, 0.83, 0.86, 0.88, 0.91, 0.94, 0.96,
        ];
        let signal = detect_bid_signal(&prices, 0.70, &cfg()).expect("signal");

        assert_eq!(signal.recommendation, BidRecommendation::MakerUp);
        assert!(signal.fair_probability > 0.75, "{signal:?}");
        assert!(signal.delta > 0.05, "{signal:?}");
        assert!(signal.expected_value > 0.0, "{signal:?}");
        assert!(signal.kelly_fraction > 0.0, "{signal:?}");
        assert!(signal.kelly_fraction <= cfg().max_kelly_fraction);
    }

    #[test]
    fn skips_when_market_price_is_not_mispriced() {
        let prices = [0.54, 0.56, 0.55, 0.57, 0.56, 0.58, 0.57, 0.56, 0.58];
        let signal = detect_bid_signal(&prices, 0.99, &cfg()).expect("signal");

        assert_eq!(signal.recommendation, BidRecommendation::Skip);
        assert!(signal.expected_value <= 0.0 || signal.delta <= cfg().min_delta);
    }

    #[test]
    fn optimism_tax_prefers_no_for_cheap_yes_contracts() {
        let prices = [0.18, 0.16, 0.14, 0.12, 0.10, 0.08, 0.07, 0.05, 0.04];
        let signal = detect_bid_signal(&prices, 0.20, &cfg()).expect("signal");

        assert_eq!(signal.recommendation, BidRecommendation::MakerDown);
        assert!(signal.fair_probability < 0.20, "{signal:?}");
        assert!(signal.no_expected_value > 0.0, "{signal:?}");
        assert!(signal.no_kelly_fraction > 0.0, "{signal:?}");
    }

    #[test]
    fn spot_context_boosts_up_when_spot_is_above_target_late_in_window() {
        let prices = [0.44, 0.45, 0.44, 0.45, 0.44, 0.45, 0.44, 0.45, 0.44];
        let ctx = DetectionContext {
            outcome: DetectionOutcome::Up,
            spot_price: 101.0,
            price_to_beat: 100.0,
            elapsed_secs: 270,
            window_secs: 300,
        };
        let signal = detect_bid_signal_with_context(&prices, 0.48, &ctx, &cfg()).expect("signal");

        assert_eq!(signal.recommendation, BidRecommendation::MakerUp);
        assert!(signal.fair_probability > 0.55, "{signal:?}");
        assert!(signal.context_probability > 0.75, "{signal:?}");
    }

    #[test]
    fn spot_context_boosts_down_when_spot_is_below_target_late_in_window() {
        let prices = [0.42, 0.43, 0.42, 0.43, 0.42, 0.43, 0.42, 0.43, 0.42];
        let ctx = DetectionContext {
            outcome: DetectionOutcome::Down,
            spot_price: 99.0,
            price_to_beat: 100.0,
            elapsed_secs: 270,
            window_secs: 300,
        };
        let signal = detect_bid_signal_with_context(&prices, 0.47, &ctx, &cfg()).expect("signal");

        assert_eq!(signal.recommendation, BidRecommendation::MakerUp);
        assert!(signal.fair_probability > 0.55, "{signal:?}");
        assert!(signal.context_probability > 0.75, "{signal:?}");
    }

    #[test]
    fn markov_prior_teaches_cross_bucket_paths_during_runtime() {
        let prices = [0.41, 0.42, 0.43, 0.44, 0.45, 0.44, 0.45, 0.44];
        let cold = detect_bid_signal(&prices, 0.44, &cfg()).expect("cold signal");
        assert!(cold.markov_probability < 0.01, "{cold:?}");

        let mut prior = MarkovPrior::default();
        for _ in 0..20 {
            prior.record_transition(0.44, 0.56);
            prior.record_transition(0.56, 0.62);
        }
        let warm = detect_bid_signal_with_prior(&prices, 0.44, &prior, &cfg())
            .expect("warm signal");

        assert!(warm.markov_probability > 0.50, "{warm:?}");
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
}
