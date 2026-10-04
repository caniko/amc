//! Bounded chronological replay of supervisor traces. Replay never actuates and
//! never fabricates post-intervention outcomes or coverage for missing frames.
use crate::{
    forecast::{Counts, Forecast},
    native::{Boundary, forecast_input},
    policy::Policy,
    recovery::Identity,
};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{BufRead, Read},
};

#[derive(Deserialize)]
struct Frame {
    version: u32,
    boot_id: String,
    observed_boot_ms: u64,
    policy: Policy,
    domains: Vec<Domain>,
}
#[derive(Deserialize)]
struct Domain {
    id: String,
    status: String,
    identity: Option<Identity>,
    boundaries: Vec<Boundary>,
}
#[derive(Default, Serialize)]
pub struct Summary {
    pub observations: u64,
    pub possible: u64,
    pub strong: u64,
    pub completed_windows: u64,
    pub evaluated_windows: u64,
    pub uncovered_windows: u64,
    pub censored_windows: u64,
}
struct Stream {
    forecast: Forecast,
    policy: String,
    boot: String,
    summary: Summary,
    counted: Counts,
}

impl Stream {
    fn tally(&mut self) {
        let counts = self.forecast.counts();
        self.summary.completed_windows += counts.completed - self.counted.completed;
        self.summary.censored_windows += counts.censored - self.counted.censored;
        self.summary.evaluated_windows += counts.evaluated - self.counted.evaluated;
        self.summary.uncovered_windows += counts.uncovered - self.counted.uncovered;
        self.counted = counts;
    }
    fn censor(&mut self) {
        self.forecast.censor();
        self.tally();
    }
}

pub fn run(mut input: impl BufRead) -> Result<BTreeMap<String, Summary>> {
    let mut streams: BTreeMap<String, Stream> = BTreeMap::new();
    let mut total = 0usize;
    let mut last_frame: Option<(String, u64)> = None;
    let mut boots = BTreeSet::new();
    loop {
        let mut line = String::new();
        let n = (&mut input).take(1_048_577).read_line(&mut line)?;
        if n == 0 {
            break;
        }
        total = total
            .checked_add(n)
            .ok_or_else(|| anyhow::anyhow!("replay size overflow"))?;
        ensure!(
            n <= 1_048_576 && total <= 128 * 1024 * 1024,
            "replay exceeds frame/total byte bound"
        );
        let frame: Frame = serde_json::from_str(&line)?;
        ensure!(
            frame.version == 1 && frame.boot_id.len() == 36 && frame.domains.len() <= 96,
            "invalid replay frame"
        );
        frame.policy.validate()?;
        if let Some((boot, ms)) = &last_frame {
            ensure!(
                boot != &frame.boot_id || frame.observed_boot_ms > *ms,
                "non-chronological replay"
            );
            ensure!(
                boot == &frame.boot_id || !boots.contains(&frame.boot_id),
                "interleaved replay boots"
            );
        }
        boots.insert(frame.boot_id.clone());
        last_frame = Some((frame.boot_id.clone(), frame.observed_boot_ms));
        let settings = serde_json::to_string(&frame.policy.forecast)?;
        let mut present = BTreeSet::new();
        for domain in frame.domains {
            ensure!(
                amc_admission::ledger::valid_name(&domain.id) && domain.boundaries.len() <= 32,
                "invalid replay domain"
            );
            ensure!(present.insert(domain.id.clone()), "duplicate replay domain");
            ensure!(
                streams.len() < 256 || streams.contains_key(&domain.id),
                "replay domain count exceeds bound"
            );
            let stream = streams.entry(domain.id).or_insert(Stream {
                forecast: Forecast::new(frame.policy.forecast)?,
                policy: settings.clone(),
                boot: frame.boot_id.clone(),
                summary: Summary::default(),
                counted: Counts::default(),
            });
            if stream.boot != frame.boot_id || stream.policy != settings {
                stream.censor();
                stream.forecast = Forecast::new(frame.policy.forecast)?;
                stream.counted = Counts::default();
                stream.policy = settings.clone();
                stream.boot = frame.boot_id.clone();
            }
            if !matches!(domain.status.as_str(), "observed" | "warming")
                || domain.boundaries.is_empty()
                || domain.identity.is_none()
            {
                stream.censor();
                continue;
            }
            let identity = domain
                .identity
                .ok_or_else(|| anyhow::anyhow!("missing replay identity"))?;
            let (signature, values) = forecast_input(&identity, &domain.boundaries)?;
            let report = stream
                .forecast
                .observe(frame.observed_boot_ms, &signature, &values)?;
            stream.summary.observations += 1;
            stream.summary.possible += u64::from(report.possible);
            stream.summary.strong += u64::from(report.strong);
            stream.tally();
        }
        for (id, stream) in &mut streams {
            if !present.contains(id) {
                stream.censor();
            }
        }
    }
    // EOF ends every comparable segment, including windows whose future is
    // absent from a cleanly truncated or rotated trace. Never omit those from
    // the retained censorship counts or score invented future observations.
    for stream in streams.values_mut() {
        stream.censor();
    }
    Ok(streams.into_iter().map(|(id, s)| (id, s.summary)).collect())
}
