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
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryStats {
    domains: Vec<MemoryDomain>,
    page_cache_bytes: u64,
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
        page_cache_bytes: u64,
    ) -> Result<Self, ProviderError> {
        if domains.is_empty() {
            return Err(ProviderError::new("no memory domains"));
        }
        Ok(Self {
            domains,
            page_cache_bytes,
        })
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

    /// Leaf page-cache bytes, or 0 if unknown.
    #[must_use]
    pub fn page_cache_bytes(&self) -> u64 {
        self.page_cache_bytes
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
