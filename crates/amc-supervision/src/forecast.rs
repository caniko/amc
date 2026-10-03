//! Prequential, bounded forecasting. Scores are maxima over the *whole* future
//! window and all supplied boundaries, never independent per-second intervals.
//! Intervals describe usage, not OOM probabilities. Calibration diagnostics are
//! empirical: arbitrary nonstationary resource streams have no promised local
//! or conditional coverage. See docs/supervision.md for the exact assumptions.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub horizon: usize,
    pub calibration: usize,
    pub alpha: f64,
    pub max_gap_ms: u64,
}

impl Settings {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (2..=120).contains(&self.horizon),
            "horizon must be 2..120 samples"
        );
        ensure!(
            (32..=2048).contains(&self.calibration),
            "calibration must be 32..2048 windows"
        );
        ensure!(
            self.alpha.is_finite() && (0.001..=0.2).contains(&self.alpha),
            "invalid miscoverage target"
        );
        ensure!(
            (100..=120_000).contains(&self.max_gap_ms),
            "invalid sampling gap bound"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Report {
    pub ready: bool,
    pub possible: bool,
    pub strong: bool,
    pub upper_peak: Vec<f64>,
    pub lower_peak: Vec<f64>,
    pub completed_windows: u64,
    pub censored_windows: u64,
    pub evaluated_windows: u64,
    pub uncovered_windows: u64,
    pub calibration_windows: usize,
    pub e_cusum: f64,
}

#[derive(Clone, Copy, Default)]
pub struct Counts {
    pub completed: u64,
    pub censored: u64,
    pub evaluated: u64,
    pub uncovered: u64,
}

/// Three switching local-linear state-space experts: plateau, damped growth,
/// sustained growth. Student-t innovations bound outlier influence. Fixed-share
/// likelihood weights retain the ability to switch after a regime change.
struct Expert {
    level: f64,
    velocity: f64,
    covariance: [f64; 3],
    damping: f64,
    weight: f64,
}

impl Expert {
    fn update(&mut self, value: f64) -> f64 {
        let d = self.damping;
        self.level += d * self.velocity;
        self.velocity *= d;
        let [a, b, c] = self.covariance;
        let p = [
            a + 2.0 * d * b + d * d * c + 1e-6,
            d * b + d * d * c,
            d * d * c + 1e-7,
        ];
        let residual = value - self.level;
        let variance = p[0] + 1e-5;
        let z2 = residual * residual / variance;
        let log_likelihood = -0.5 * variance.ln() - 2.5 * (1.0 + z2 / 4.0).ln();
        let robust_noise = (1e-5 * (4.0 + z2) / 5.0).max(1e-7);
        let k0 = p[0] / (p[0] + robust_noise);
        let k1 = p[1] / (p[0] + robust_noise);
        self.level += k0 * residual;
        self.velocity += k1 * residual;
        self.covariance = [
            (p[0] * (1.0 - k0)).max(1e-12),
            p[1] * (1.0 - k0),
            (p[2] - k1 * p[1]).max(1e-12),
        ];
        log_likelihood
    }

    fn path(&self, horizon: usize) -> Vec<f64> {
        let mut level = self.level;
        let mut velocity = self.velocity;
        (0..horizon)
            .map(|_| {
                velocity *= self.damping;
                level += velocity;
                level
            })
            .collect()
    }
}

struct Ensemble(Vec<Expert>);
impl Ensemble {
    fn new(value: f64) -> Self {
        Self(
            [0.0, 0.85, 1.0]
                .into_iter()
                .map(|damping| Expert {
                    level: value,
                    velocity: 0.0,
                    covariance: [1e-4, 0.0, 1e-4],
                    damping,
                    weight: 1.0 / 3.0,
                })
                .collect(),
        )
    }

    fn update(&mut self, value: f64) {
        let log_weights: Vec<_> = self
            .0
            .iter_mut()
            .map(|expert| expert.weight.ln() + expert.update(value))
            .collect();
        let max = log_weights
            .iter()
            .copied()
            .fold(f64::NEG_INFINITY, f64::max);
        let weights: Vec<_> = log_weights.iter().map(|w| (w - max).exp()).collect();
        let total: f64 = weights.iter().sum();
        for (expert, weight) in self.0.iter_mut().zip(weights) {
            expert.weight = 0.97 * weight / total + 0.01;
        }
    }

    fn path(&self, horizon: usize) -> Vec<f64> {
        let mut out = vec![0.0; horizon];
        for expert in &self.0 {
            for (target, prediction) in out.iter_mut().zip(expert.path(horizon)) {
                *target += expert.weight * prediction;
            }
        }
        out
    }
}

struct Window {
    predictions: Vec<Vec<f64>>,
    age: usize,
    score: f64,
    quantile: Option<f64>,
}

/// Bounded mixture of e-CUSUM processes. The null is explicitly
/// E[clip(normalized innovation, -1, 1) | past] <= 0. Under that null the
/// detector has an ARL bound, not an ever-alarm probability or a leak diagnosis.
#[derive(Default)]
pub struct EDetector([f64; 3]);
impl EDetector {
    pub fn observe(&mut self, innovation: f64) -> Result<f64> {
        ensure!(innovation.is_finite(), "non-finite innovation");
        for (value, bet) in self.0.iter_mut().zip([0.1, 0.25, 0.5]) {
            *value = (value.max(1.0) * (1.0 + bet * innovation.clamp(-1.0, 1.0))).min(1e100);
        }
        Ok(self.0.iter().sum::<f64>() / 3.0)
    }
}

pub struct Forecast {
    settings: Settings,
    identity: String,
    last_ms: Option<u64>,
    previous: Vec<f64>,
    models: Vec<Ensemble>,
    pending: VecDeque<Window>,
    scores: VecDeque<f64>,
    integral: f64,
    detector: EDetector,
    completed: u64,
    censored: u64,
    evaluated: u64,
    uncovered: u64,
}

impl Forecast {
    pub fn new(settings: Settings) -> Result<Self> {
        settings.validate()?;
        Ok(Self {
            settings,
            identity: String::new(),
            last_ms: None,
            previous: Vec::new(),
            models: Vec::new(),
            pending: VecDeque::new(),
            scores: VecDeque::new(),
            integral: 0.0,
            detector: EDetector::default(),
            completed: 0,
            censored: 0,
            evaluated: 0,
            uncovered: 0,
        })
    }

    /// End a comparable segment on a missing sample or an intervention. No
    /// counterfactual future is scored, and calibration cannot survive a restart.
    pub fn censor(&mut self) {
        self.censored += self.pending.len() as u64;
        self.pending.clear();
        self.scores.clear();
        self.models.clear();
        self.previous.clear();
        self.integral = 0.0;
        self.detector = EDetector::default();
        self.last_ms = None;
    }

    pub fn counts(&self) -> Counts {
        Counts {
            completed: self.completed,
            censored: self.censored,
            evaluated: self.evaluated,
            uncovered: self.uncovered,
        }
    }

    fn quantile(&self) -> Option<f64> {
        if self.scores.len() < self.settings.calibration {
            return None;
        }
        let mut scores: Vec<_> = self.scores.iter().copied().collect();
        scores.sort_by(f64::total_cmp);
        let rank = (((scores.len() + 1) as f64 * (1.0 - self.settings.alpha)).ceil() as usize)
            .saturating_sub(1);
        // An unestimable tail is unknown; do not silently use the largest score.
        let empirical = *scores.get(rank)?;
        // Integral coverage-error feedback, as in online conformal PID control.
        // Empty intervals remain explicit and cannot authorize actuation.
        Some(empirical + 0.0001 * self.integral)
    }

    /// Values are usage / protected boundary. Boundary selection and identity
    /// verification belong to the native observer, not this statistical core.
    pub fn observe(&mut self, ms: u64, identity: &str, values: &[f64]) -> Result<Report> {
        ensure!(
            !identity.is_empty() && identity.len() <= 8192,
            "invalid forecast identity"
        );
        ensure!(
            !values.is_empty()
                && values.len() <= 32
                && values
                    .iter()
                    .all(|x| x.is_finite() && (0.0..=1e6).contains(x)),
            "invalid boundary observation"
        );
        if let Some(last) = self.last_ms {
            ensure!(ms > last, "non-monotonic observation");
        }
        let reset = self.identity != identity
            || self.models.len() != values.len()
            || self
                .last_ms
                .is_some_and(|last| ms - last > self.settings.max_gap_ms);
        if reset {
            self.censor();
            self.identity = identity.into();
        }
        for window in &mut self.pending {
            for (predictions, value) in window.predictions.iter().zip(values) {
                window.score = window.score.max((predictions[window.age] - value).abs());
            }
            window.age += 1;
        }
        while self
            .pending
            .front()
            .is_some_and(|w| w.age == self.settings.horizon)
        {
            let window = self
                .pending
                .pop_front()
                .ok_or_else(|| anyhow::anyhow!("missing forecast window"))?;
            self.completed += 1;
            if let Some(q) = window.quantile {
                let miss = window.score > q;
                self.integral += f64::from(miss) - self.settings.alpha;
                self.evaluated += 1;
                self.uncovered += u64::from(miss);
            }
            self.scores.push_back(window.score);
            if self.scores.len() > self.settings.calibration {
                self.scores.pop_front();
            }
        }
        let mut evidence = 0.0;
        if self.models.is_empty() {
            self.models = values.iter().map(|v| Ensemble::new(*v)).collect();
        } else {
            // Diagnostic only; normal workload growth can also supply evidence.
            evidence = self.detector.observe(
                values
                    .iter()
                    .zip(&self.previous)
                    .map(|(value, previous)| value - previous)
                    .fold(f64::NEG_INFINITY, f64::max)
                    * 100.0,
            )?;
            for (model, value) in self.models.iter_mut().zip(values) {
                model.update(*value);
            }
        }
        let predictions: Vec<_> = self
            .models
            .iter()
            .map(|m| m.path(self.settings.horizon))
            .collect();
        let q = self.quantile();
        let ready = q.is_some_and(|q| q.is_finite() && q >= 0.0);
        let peaks: Vec<_> = predictions
            .iter()
            .map(|path| path.iter().copied().fold(f64::NEG_INFINITY, f64::max))
            .collect();
        let radius = q.filter(|q| *q >= 0.0).unwrap_or(0.0);
        let upper_peak: Vec<_> = peaks.iter().map(|peak| peak + radius).collect();
        let lower_peak: Vec<_> = peaks.iter().map(|peak| peak - radius).collect();
        let possible = upper_peak.iter().any(|v| *v >= 1.0);
        let strong = ready && lower_peak.iter().any(|v| *v >= 1.0);
        self.pending.push_back(Window {
            predictions,
            age: 0,
            score: 0.0,
            quantile: q,
        });
        self.previous = values.to_vec();
        self.last_ms = Some(ms);
        Ok(Report {
            ready,
            possible,
            strong,
            upper_peak,
            lower_peak,
            completed_windows: self.completed,
            censored_windows: self.censored,
            evaluated_windows: self.evaluated,
            uncovered_windows: self.uncovered,
            calibration_windows: self.scores.len(),
            e_cusum: evidence,
        })
    }
}
