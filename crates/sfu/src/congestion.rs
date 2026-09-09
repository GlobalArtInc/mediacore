use std::time::{Duration, Instant};

use crate::bitrate::Bitrate;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CongestionState {
    #[default]
    Clear,
    Hold,
    Congested,
}

impl CongestionState {
    pub fn is_clear(self) -> bool {
        matches!(self, Self::Clear)
    }

    pub fn is_congested(self) -> bool {
        matches!(self, Self::Congested)
    }
}

impl std::fmt::Display for CongestionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Clear => write!(f, "clear"),
            Self::Hold => write!(f, "hold"),
            Self::Congested => write!(f, "congested"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Signal {
    Pressure,
    Relief,
    #[default]
    Indeterminate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Signals {
    pub estimate: Signal,
    pub loss: Signal,
    pub queue: Signal,
}

impl Signals {
    pub fn combined(&self) -> Signal {
        if self.backlog() {
            return Signal::Pressure;
        }

        if self.each().contains(&Signal::Pressure) {
            return Signal::Indeterminate;
        }

        if self.each().contains(&Signal::Relief) {
            return Signal::Relief;
        }

        Signal::Indeterminate
    }

    pub fn backlog(&self) -> bool {
        self.queue == Signal::Pressure
    }

    pub fn cut_unproven(&self) -> bool {
        self.backlog() || self.loss == Signal::Pressure
    }

    pub fn corroborated_estimate(&self) -> Signal {
        Self::corroborate(self.estimate, self.backlog())
    }

    pub fn corroborated_loss(&self) -> Signal {
        Self::corroborate(self.loss, self.backlog())
    }

    fn each(&self) -> [Signal; 3] {
        [self.estimate, self.loss, self.queue]
    }

    fn corroborate(signal: Signal, backlog: bool) -> Signal {
        match signal {
            Signal::Pressure if backlog => Signal::Pressure,
            Signal::Pressure => Signal::Indeterminate,
            other => other,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CongestionConfig {
    pub initial: Bitrate,
    pub offer_pressure_percent: u64,
    pub offer_relief_percent: u64,
    pub loss_min_packets: u64,
    pub queue_pressure: Duration,
    pub queue_relief: Duration,
    pub hold_entry: u32,
    pub congested_entry: u32,
    pub hold_exit: u32,
    pub claim_lapse: Duration,
    pub commit_percent: u64,
    pub recut_percent: u64,
    pub recut_delay: Duration,
    pub recut_rtt_multiple: u32,
    pub min_budget: Bitrate,
    pub loss_pressure: f64,
    pub loss_relief: f64,
}

impl Default for CongestionConfig {
    fn default() -> Self {
        Self {
            initial: Bitrate::kbps(600),
            offer_pressure_percent: 85,
            offer_relief_percent: 95,
            loss_min_packets: 30,
            queue_pressure: Duration::from_millis(150),
            queue_relief: Duration::from_millis(30),
            hold_entry: 1,
            congested_entry: 2,
            hold_exit: 3,
            claim_lapse: Duration::from_secs(5),
            commit_percent: 95,
            recut_percent: 85,
            recut_delay: Duration::from_secs(2),
            recut_rtt_multiple: 5,
            min_budget: Bitrate::kbps(150),
            loss_pressure: 0.25,
            loss_relief: 0.10,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sample {
    pub at: Instant,
    pub raw: Option<Bitrate>,
    pub loss: Option<f32>,
    pub rtt: Option<Duration>,
    pub sent: Bitrate,
    pub packets: u64,
    pub queue_age: Duration,
    pub queue_bytes: usize,
}

impl Sample {
    pub fn new(at: Instant) -> Self {
        Self {
            at,
            raw: None,
            loss: None,
            rtt: None,
            sent: Bitrate::ZERO,
            packets: 0,
            queue_age: Duration::ZERO,
            queue_bytes: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Verdict {
    pub state: CongestionState,
    pub changed: bool,
    pub budget: Bitrate,
    pub reallocate: bool,
    pub signals: Signals,
}

#[derive(Debug, Clone)]
pub struct CongestionMonitor {
    config: CongestionConfig,
    state: CongestionState,
    since: Option<Instant>,
    pressure_run: u32,
    relief_run: u32,
    last_pressure: Option<Instant>,
    estimate: Option<Bitrate>,
    committed: Bitrate,
    signals: Signals,
    last_sample: Option<Instant>,
    last_recut: Option<Instant>,
}

impl CongestionMonitor {
    pub fn new(config: CongestionConfig) -> Self {
        Self {
            committed: config.initial,
            config,
            state: CongestionState::Clear,
            since: None,
            pressure_run: 0,
            relief_run: 0,
            last_pressure: None,
            estimate: None,
            signals: Signals::default(),
            last_sample: None,
            last_recut: None,
        }
    }

    pub fn config(&self) -> &CongestionConfig {
        &self.config
    }

    pub fn state(&self) -> CongestionState {
        self.state
    }

    pub fn state_since(&self) -> Option<Instant> {
        self.since
    }

    pub fn committed(&self) -> Bitrate {
        self.committed
    }

    pub fn estimate(&self) -> Option<Bitrate> {
        self.estimate
    }

    pub fn signals(&self) -> Signals {
        self.signals
    }

    pub fn on_estimate(&mut self, bitrate: Bitrate, _at: Instant) {
        self.estimate = Some(bitrate);
    }

    pub fn on_sample(&mut self, sample: Sample, spend: Bitrate) -> Verdict {
        let signals = self.classify(&sample, spend);
        self.signals = signals;

        let before = self.state;
        self.advance(signals.combined(), sample.at);
        let changed = self.state != before;

        let reallocate = match self.state {
            CongestionState::Clear => {
                if changed {
                    self.last_recut = None;
                }
                self.settle(&sample, spend) || changed
            }
            CongestionState::Hold => false,
            CongestionState::Congested => self.on_congested(&sample, spend, changed, signals),
        };

        self.last_sample = Some(sample.at);

        Verdict {
            state: self.state,
            changed,
            budget: self.budget(),
            reallocate,
            signals,
        }
    }

    pub fn budget(&self) -> Bitrate {
        self.committed
    }

    pub fn reset(&mut self) {
        self.state = CongestionState::Clear;
        self.since = None;
        self.pressure_run = 0;
        self.relief_run = 0;
        self.last_pressure = None;
        self.committed = self.config.initial;
        self.signals = Signals::default();
        self.last_sample = None;
        self.last_recut = None;
    }

    fn on_congested(
        &mut self,
        sample: &Sample,
        spend: Bitrate,
        changed: bool,
        signals: Signals,
    ) -> bool {
        if changed {
            self.last_recut = Some(sample.at);
            let candidate = self.first_cut(sample);

            return self.commit(candidate.min(self.committed), spend, sample.at);
        }

        if !self.due_for_recut(sample.at, sample.rtt) {
            return false;
        }

        if !signals.cut_unproven() {
            return false;
        }

        self.last_recut = Some(sample.at);

        let leaving = sample.sent.min(self.committed);
        let candidate = leaving.scaled(self.config.recut_percent, 100);

        self.commit(candidate, spend, sample.at)
    }

    fn first_cut(&self, sample: &Sample) -> Bitrate {
        let measured = sample.sent.scaled(self.config.recut_percent, 100);

        match (sample.raw, sample.sent.is_zero()) {
            (Some(raw), false) => raw.min(measured),
            (Some(raw), true) => raw,
            (None, _) => measured,
        }
    }

    fn settle(&mut self, sample: &Sample, spend: Bitrate) -> bool {
        let Some(estimate) = self.estimate.or(sample.raw) else {
            return false;
        };

        let candidate = estimate.max(self.config.min_budget);

        match candidate.cmp(&self.committed) {
            std::cmp::Ordering::Equal => false,
            std::cmp::Ordering::Greater => {
                tracing::debug!(
                    from = %self.committed,
                    to = %candidate,
                    "raising a subscriber bandwidth budget on a clear link"
                );

                self.committed = candidate;
                true
            }
            std::cmp::Ordering::Less => self.commit(candidate, spend, sample.at),
        }
    }

    fn due_for_recut(&self, at: Instant, rtt: Option<Duration>) -> bool {
        let Some(last) = self.last_recut else {
            return true;
        };

        let floor = rtt
            .map(|rtt| rtt.saturating_mul(self.config.recut_rtt_multiple))
            .unwrap_or(Duration::ZERO)
            .max(self.config.recut_delay);

        at.saturating_duration_since(last) >= floor
    }

    fn commit(&mut self, candidate: Bitrate, spend: Bitrate, at: Instant) -> bool {
        let candidate = candidate.max(self.config.min_budget);

        if candidate > spend.scaled(self.config.commit_percent, 100) {
            tracing::debug!(
                %candidate,
                %spend,
                "declining a budget that would not reduce what we already spend"
            );
            return false;
        }

        if self.committed == candidate {
            return false;
        }

        tracing::debug!(
            from = %self.committed,
            to = %candidate,
            state = %self.state,
            "committing a subscriber bandwidth budget"
        );

        self.committed = candidate;
        self.last_recut = Some(at);
        true
    }

    fn advance(&mut self, signal: Signal, at: Instant) {
        match signal {
            Signal::Pressure => {
                self.pressure_run += 1;
                self.relief_run = 0;
                self.last_pressure = Some(at);
            }
            Signal::Relief => {
                self.relief_run += 1;
                self.pressure_run = 0;
            }
            Signal::Indeterminate => {
                self.pressure_run = 0;
                self.relief_run = 0;
            }
        }

        let next = match self.state {
            CongestionState::Clear if self.pressure_run >= self.config.hold_entry => {
                Some(CongestionState::Hold)
            }
            CongestionState::Hold if self.pressure_run >= self.config.congested_entry => {
                Some(CongestionState::Congested)
            }
            CongestionState::Hold if self.relief_run >= self.config.hold_exit => {
                Some(CongestionState::Clear)
            }
            CongestionState::Hold if self.lapsed(at, self.since) => Some(CongestionState::Clear),
            CongestionState::Congested if self.lapsed(at, self.last_pressure) => {
                Some(CongestionState::Clear)
            }
            _ => None,
        };

        let Some(next) = next else {
            return;
        };

        tracing::debug!(from = %self.state, to = %next, "subscriber congestion state changed");

        self.state = next;
        self.since = Some(at);
        self.pressure_run = 0;
        self.relief_run = 0;
    }

    fn lapsed(&self, at: Instant, from: Option<Instant>) -> bool {
        from.is_some_and(|from| at.saturating_duration_since(from) >= self.config.claim_lapse)
    }

    fn classify(&self, sample: &Sample, spend: Bitrate) -> Signals {
        Signals {
            estimate: self.estimate_signal(sample, spend),
            loss: self.loss_signal(sample),
            queue: self.queue_signal(sample),
        }
    }

    fn estimate_signal(&self, sample: &Sample, spend: Bitrate) -> Signal {
        let Some(raw) = sample.raw else {
            return Signal::Indeterminate;
        };

        if spend.is_zero() {
            return Signal::Indeterminate;
        }

        if raw < spend.scaled(self.config.offer_pressure_percent, 100) {
            return Signal::Pressure;
        }

        if raw >= spend.scaled(self.config.offer_relief_percent, 100) {
            return Signal::Relief;
        }

        Signal::Indeterminate
    }

    fn loss_signal(&self, sample: &Sample) -> Signal {
        let Some(loss) = sample.loss else {
            return Signal::Indeterminate;
        };

        let Some(previous) = self.last_sample else {
            return Signal::Indeterminate;
        };

        let window = sample.at.saturating_duration_since(previous).as_secs_f64();
        if window <= 0.0 {
            return Signal::Indeterminate;
        }

        let weighted = if sample.packets < self.config.loss_min_packets {
            0.0
        } else {
            let pps = sample.packets as f64 / window;
            f64::from(loss.max(0.0)) * pps.log10().max(0.0)
        };

        if weighted > self.config.loss_pressure {
            return Signal::Pressure;
        }

        if weighted < self.config.loss_relief {
            return Signal::Relief;
        }

        Signal::Indeterminate
    }

    fn queue_signal(&self, sample: &Sample) -> Signal {
        if sample.queue_age > self.config.queue_pressure {
            return Signal::Pressure;
        }

        if sample.queue_age < self.config.queue_relief {
            return Signal::Relief;
        }

        Signal::Indeterminate
    }
}

impl Default for CongestionMonitor {
    fn default() -> Self {
        Self::new(CongestionConfig::default())
    }
}

#[cfg(test)]
#[path = "congestion_tests.rs"]
mod tests;
