use std::time::Duration;

const DEFAULT_MAX_RAM_FRACTION: f64 = 0.80;
const DEFAULT_RESUME_HYSTERESIS: f64 = 0.05;
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Shared configuration for the synchronous and asynchronous admission gates.
#[derive(Debug, Clone)]
pub struct Config {
    /// When the host RAM usage fraction exceeds this value, the gate stops
    /// admitting new tasks. Equality is admitted. Must lie in `(0.0, 1.0]`.
    pub max_ram_fraction: f64,

    /// Once throttled, the gate only resumes admissions when RAM usage falls
    /// below `max_ram_fraction - resume_hysteresis`. Must lie in `[0.0, max_ram_fraction)`.
    pub resume_hysteresis: f64,

    /// How often the throttled state re-checks the memory provider while
    /// blocked. Shorter values respond faster to memory release; longer values
    /// reduce wake-up overhead.
    pub poll_interval: Duration,

    /// When `false`, the gate is constructed in thread-cap-only mode and the
    /// memory provider is never consulted. Equivalent to a plain semaphore.
    pub memory_scheduler_enabled: bool,

    /// When the memory provider fails, admit anyway (advisory / fail-open).
    /// Default is `false`: acquire returns [`crate::AdmitError::Provider`].
    pub fail_open_on_provider_error: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_ram_fraction: DEFAULT_MAX_RAM_FRACTION,
            resume_hysteresis: DEFAULT_RESUME_HYSTERESIS,
            poll_interval: DEFAULT_POLL_INTERVAL,
            memory_scheduler_enabled: true,
            fail_open_on_provider_error: false,
        }
    }
}

impl Config {
    /// Return the validated default configuration.
    #[must_use]
    pub fn validated_default() -> Self {
        Self::default()
            .validate()
            .expect("default amc-runner config must be valid")
    }

    /// Validates the configuration. Returns `Ok(self)` if valid.
    ///
    /// # Errors
    /// Returns [`ConfigError`] when any field is out of its allowed range.
    pub fn validate(self) -> Result<Self, ConfigError> {
        if !(self.max_ram_fraction > 0.0 && self.max_ram_fraction <= 1.0) {
            return Err(ConfigError::MaxRamFractionOutOfRange(self.max_ram_fraction));
        }
        if !(self.resume_hysteresis >= 0.0 && self.resume_hysteresis < self.max_ram_fraction) {
            return Err(ConfigError::ResumeHysteresisOutOfRange {
                hysteresis: self.resume_hysteresis,
                max: self.max_ram_fraction,
            });
        }
        if self.poll_interval.is_zero() {
            return Err(ConfigError::PollIntervalZero);
        }
        Ok(self)
    }

    /// The threshold at which the gate resumes admissions after entering the
    /// throttled state.
    #[must_use]
    pub fn resume_ram_fraction(&self) -> f64 {
        (self.max_ram_fraction - self.resume_hysteresis).max(0.0)
    }
}

/// Errors produced when validating a [`Config`].
#[derive(Debug, Clone, PartialEq)]
pub enum ConfigError {
    /// `max_ram_fraction` must be in `(0.0, 1.0]`.
    MaxRamFractionOutOfRange(f64),

    /// `resume_hysteresis` must be in `[0.0, max_ram_fraction)`.
    ResumeHysteresisOutOfRange {
        /// The configured hysteresis value.
        hysteresis: f64,
        /// The configured max ram fraction it was compared against.
        max: f64,
    },

    /// `poll_interval` must be non-zero.
    PollIntervalZero,
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MaxRamFractionOutOfRange(v) => {
                write!(f, "max_ram_fraction must be in (0.0, 1.0], got {v}")
            }
            Self::ResumeHysteresisOutOfRange { hysteresis, max } => write!(
                f,
                "resume_hysteresis must be in [0.0, max_ram_fraction={max}), got {hysteresis}"
            ),
            Self::PollIntervalZero => f.write_str("poll_interval must be > 0"),
        }
    }
}

impl std::error::Error for ConfigError {}
