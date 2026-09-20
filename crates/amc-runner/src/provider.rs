//! Memory provider abstractions and error types.

use std::sync::Arc;

/// Reads the host's current memory state.
///
/// `used_fraction` returns a value in the closed range `[0.0, 1.0]`. The
/// optional [`MemoryProvider::stats`] returns absolute byte counts for
/// budget-aware (weighted) admission. Errors are [`ProviderError`]; the
/// gate refuses admission unless [`crate::Config::fail_open_on_provider_error`]
/// is set.
pub trait MemoryProvider: Send + Sync + 'static {
    /// Probe the current used-RAM fraction.
    ///
    /// # Errors
    /// Returns [`ProviderError`] when the underlying source is unavailable or
    /// returns inconsistent values (e.g. `MemAvailable > MemTotal`).
    fn used_fraction(&self) -> Result<f64, ProviderError>;

    /// Probe absolute memory totals.
    ///
    /// The default implementation returns `Err(ProviderError::Unsupported)`.
    /// Providers that can supply absolute counts (`/proc/meminfo`, sysinfo)
    /// override this so the weighted gates can budget by bytes rather than
    /// just fraction.
    ///
    /// # Errors
    /// Returns [`ProviderError`] when stats cannot be read.
    ///
    fn stats(&self) -> Result<MemoryStats, ProviderError> {
        Err(ProviderError::Unsupported)
    }
}

/// One observed memory domain (host or a limited cgroup).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryDomain {
    total_bytes: u64,
    available_bytes: u64,
}

impl MemoryDomain {
    /// Construct a domain. `available_bytes` must not exceed `total_bytes`.
    ///
    /// `total_bytes == 0` is a real zero-capacity limit, not unknown.
    ///
    /// # Errors
    /// Returns [`ProviderError`] when available exceeds total.
    pub fn new(total_bytes: u64, available_bytes: u64) -> Result<Self, ProviderError> {
        if available_bytes > total_bytes {
            return Err(ProviderError::new("memory domain available exceeds total"));
        }
        Ok(Self {
            total_bytes,
            available_bytes,
        })
    }

    /// Domain total in bytes.
    #[must_use]
    pub fn total_bytes(self) -> u64 {
        self.total_bytes
    }

    /// Domain headroom in bytes.
    #[must_use]
    pub fn available_bytes(self) -> u64 {
        self.available_bytes
    }

    /// Bytes in use in this domain.
    #[must_use]
    pub fn used_bytes(self) -> u64 {
        self.total_bytes.saturating_sub(self.available_bytes)
    }

    /// Used fraction for this domain. Zero-capacity domains report `1.0`.
    #[must_use]
    pub fn used_fraction(self) -> f64 {
        if self.total_bytes == 0 {
            1.0
        } else {
            1.0 - (self.available_bytes as f64 / self.total_bytes as f64)
        }
    }
}

/// Absolute memory state in bytes, one entry per observed domain.
///
/// `page_cache_bytes` is `None` when leaf cache accounting is unavailable
/// (e.g. `memory.stat` unreadable). `None` is unknown, never zero: callers
/// must not treat missing accounting as observed zero.
///
/// `page_cache_total` is the total the cache bytes belong to (the leaf
/// cgroup's effective limit, or the host total for host accounting). A
/// cache fraction must divide by this domain's total, never by an
/// unrelated aggregated minimum across independently limited domains.
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryStats {
    domains: Vec<MemoryDomain>,
    page_cache_bytes: Option<u64>,
    page_cache_total: Option<u64>,
}

impl MemoryStats {
    /// Single-domain stats.
    ///
    /// # Errors
    /// Returns [`ProviderError`] when available exceeds total.
    pub fn new(
        total_bytes: u64,
        available_bytes: u64,
        page_cache_bytes: u64,
    ) -> Result<Self, ProviderError> {
        Self::from_domains(
            vec![MemoryDomain::new(total_bytes, available_bytes)?],
            Some(page_cache_bytes),
        )
    }

    /// Single-domain stats with explicit cache knowledge. `None` means
    /// cache accounting was unavailable and must not be read as zero.
    ///
    /// # Errors
    /// Returns [`ProviderError`] when available exceeds total.
    pub fn new_with_cache(
        total_bytes: u64,
        available_bytes: u64,
        page_cache_bytes: Option<u64>,
    ) -> Result<Self, ProviderError> {
        Self::from_domains(
            vec![MemoryDomain::new(total_bytes, available_bytes)?],
            page_cache_bytes,
        )
    }

    /// Multi-domain stats. Policy is evaluated per domain, then the tightest
    /// result is used.
    ///
    /// # Errors
    /// Returns [`ProviderError`] when `domains` is empty.
    pub fn from_domains(
        domains: Vec<MemoryDomain>,
        page_cache_bytes: impl Into<Option<u64>>,
    ) -> Result<Self, ProviderError> {
        if domains.is_empty() {
            return Err(ProviderError::new("no memory domains"));
        }
        Ok(Self {
            domains,
            page_cache_bytes: page_cache_bytes.into(),
            page_cache_total: None,
        })
    }

    /// Attach the total the cache bytes belong to. Single-domain providers
    /// pass their own total; the cgroup provider passes the leaf's
    /// effective limit (or the host total when the leaf is unlimited).
    #[must_use]
    pub fn with_cache_total(mut self, total_bytes: u64) -> Self {
        self.page_cache_total = Some(total_bytes);
        self
    }

    /// Observed domains, host first when present.
    #[must_use]
    pub fn domains(&self) -> &[MemoryDomain] {
        &self.domains
    }

    /// Tightest total across domains.
    #[must_use]
    pub fn total_bytes(&self) -> u64 {
        self.domains
            .iter()
            .map(|d| d.total_bytes)
            .min()
            .unwrap_or(0)
    }

    /// Tightest headroom across domains.
    #[must_use]
    pub fn available_bytes(&self) -> u64 {
        self.domains
            .iter()
            .map(|d| d.available_bytes)
            .min()
            .unwrap_or(0)
    }

    /// Leaf page-cache bytes, or 0 if unknown (legacy compat).
    ///
    /// Prefer [`Self::page_cache_opt`]: a return of 0 here conflates
    /// observed zero with unavailable accounting.
    #[must_use]
    pub fn page_cache_bytes(&self) -> u64 {
        self.page_cache_bytes.unwrap_or(0)
    }

    /// Leaf page-cache bytes with explicit unknown. `None` must not be
    /// treated as observed zero; when a policy requires this measurement,
    /// callers use an explicit failure policy, otherwise they skip the
    /// cache check without invalidating the observation.
    #[must_use]
    pub fn page_cache_opt(&self) -> Option<u64> {
        self.page_cache_bytes
    }

    /// Whether cache accounting is known.
    #[must_use]
    pub fn page_cache_known(&self) -> bool {
        self.page_cache_bytes.is_some()
    }

    /// Total the cache bytes belong to, if the provider reported one.
    /// Callers without an explicit total must fall back to their own
    /// single domain total — never to a minimum aggregated across
    /// independently limited domains.
    #[must_use]
    pub fn page_cache_total(&self) -> Option<u64> {
        self.page_cache_total
    }

    /// Infer the total for cache accounting when none was explicitly attached.
    /// Returns the explicit total if present, or the single domain's total
    /// if there is exactly one domain. Otherwise returns `None`.
    #[must_use]
    pub fn infer_cache_total(&self) -> Option<u64> {
        self.page_cache_total
            .or_else(|| (self.domains.len() == 1).then(|| self.domains[0].total_bytes))
    }

    /// Highest used fraction among domains.
    #[must_use]
    pub fn used_fraction(&self) -> f64 {
        self.domains
            .iter()
            .map(|d| d.used_fraction())
            .fold(0.0, f64::max)
    }
}

/// Require a finite usage fraction in `[0.0, 1.0]`.
pub fn finite_fraction(usage: f64) -> Result<f64, ProviderError> {
    if usage.is_finite() && (0.0..=1.0).contains(&usage) {
        Ok(usage)
    } else {
        Err(ProviderError::new(
            "memory usage is not a finite fraction in [0, 1]",
        ))
    }
}

/// Boxed memory provider — type alias for ergonomic storage in gates.
pub type SharedMemoryProvider = Arc<dyn MemoryProvider>;

/// Error produced by a [`MemoryProvider`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderError {
    /// Underlying source is unavailable, malformed, or inconsistent.
    Source(String),
    /// The provider does not support the requested operation
    /// (e.g. [`MemoryProvider::stats`] when the implementation is
    /// fraction-only).
    Unsupported,
}

impl ProviderError {
    /// Construct a `Source` error from any displayable value.
    pub fn new(msg: impl Into<String>) -> Self {
        Self::Source(msg.into())
    }
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Source(s) => f.write_str(s),
            Self::Unsupported => f.write_str("memory provider does not support this operation"),
        }
    }
}

impl std::error::Error for ProviderError {}

impl<F> MemoryProvider for F
where
    F: Fn() -> Result<f64, ProviderError> + Send + Sync + 'static,
{
    fn used_fraction(&self) -> Result<f64, ProviderError> {
        finite_fraction((self)()?)
    }
}
