//! Source identity and lifetime comparability.
//!
//! An observation ID names a collection session, never a workload. Counter
//! deltas additionally require compatible lifetime: same cgroup path plus
//! continuity evidence (invocation ID when available, inode as a hint, never
//! as proof alone). Missing identity in legacy JSON is never guessed.

use serde::{Deserialize, Serialize};

/// Bounded string helper. Returns `None` when `value` exceeds `limit`:
/// overlong identities are rejected, never truncated into a possible
/// collision with a different identity.
fn bound(value: &str, limit: usize) -> Option<String> {
    if value.len() <= limit {
        Some(value.to_string())
    } else {
        None
    }
}

/// Where a snapshot came from. All optional fields use explicit unknown
/// rather than empty strings or zeros.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SourceIdentity {
    pub cgroup_path: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub unit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub manager_context: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub uid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub boot_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub invocation_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub inode: Option<u64>,
}

impl SourceIdentity {
    /// Build from caller-supplied verified values. Core never shells out to
    /// systemctl; the observer resolves invocation IDs and passes them in.
    /// All strings are bounded to keep JSON small and prose-free.
    pub fn new(
        cgroup_path: &str,
        unit: Option<&str>,
        manager_context: Option<&str>,
        uid: Option<u32>,
        boot_id: Option<&str>,
        invocation_id: Option<&str>,
        inode: Option<u64>,
    ) -> Self {
        Self {
            // `cgroup_path` is required: fall back to empty (which can never
            // compare compatible) rather than truncating into a collision.
            cgroup_path: bound(cgroup_path, 512).unwrap_or_default(),
            unit: unit.and_then(|v| bound(v, 256)),
            manager_context: manager_context.and_then(|v| bound(v, 16)),
            uid,
            boot_id: boot_id.and_then(|v| bound(v, 64)),
            invocation_id: invocation_id.and_then(|v| bound(v, 64)),
            inode,
        }
    }

    /// Read the cgroup directory inode as a replacement hint (not proof).
    /// Open handles do not guarantee counter retention; inode match alone is
    /// never conclusive.
    pub fn inode_of(dir: &std::path::Path) -> Option<u64> {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(dir).ok().map(|m| m.ino())
    }

    /// Best-effort boot ID, bounded. Absent on non-Linux, when unreadable,
    /// or when overlong (rejected, never truncated).
    pub fn read_boot_id() -> Option<String> {
        std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .ok()
            .and_then(|v| bound(v.trim(), 64))
            .filter(|v| !v.is_empty())
    }
}

/// A collection session: many samples share one observation ID.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ObservationSession {
    pub observation_id: String,
    pub started_unix_ms: Option<u128>,
    pub started_monotonic_ms: u64,
    pub clock_domain: &'static str,
}

impl ObservationSession {
    pub fn new(
        observation_id: &str,
        started_unix_ms: Option<u128>,
        started_monotonic_ms: u64,
    ) -> Self {
        Self {
            observation_id: bound(observation_id, 64).unwrap_or_default(),
            started_unix_ms,
            started_monotonic_ms,
            clock_domain: "monotonic-clock",
        }
    }
}

/// Lifetime comparability verdict for a before/after pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Comparability {
    /// Same path and continuity evidence agree: counters may be differenced.
    Compatible,
    /// Paths differ: different objects, never differenced.
    PathMismatch,
    /// Invocation IDs both present and differ: restart/replacement.
    RestartDetected,
    /// Inode changed with no invocation evidence: suspected replacement.
    ReplacementSuspected,
    /// Boot, manager context, or UID disagree: different lifetime or an
    /// observation that crossed a reboot/manager boundary.
    ContextMismatch,
    /// One side lacks identity (legacy JSON): must not guess.
    MissingIdentity,
    /// Target disappeared mid-interval.
    Disappeared,
}

impl Comparability {
    pub fn as_reason(self) -> &'static str {
        match self {
            Self::Compatible => "compatible",
            Self::PathMismatch => "lifetime-mismatch",
            Self::RestartDetected => "restart-detected",
            Self::ReplacementSuspected => "replacement-suspected",
            Self::ContextMismatch => "context-mismatch",
            Self::MissingIdentity => "missing-identity",
            Self::Disappeared => "disappeared",
        }
    }
}

/// Decide whether two samples describe the same lifetime.
///
/// `before_alive` / `after_alive` indicate the cgroup existed and was
/// readable at each endpoint. A `Compatible` verdict requires positive
/// continuity evidence: equal invocation IDs when both sides report them.
/// Path match alone is never sufficient (same path may be recycled), and a
/// boot-ID change always ends the lifetime even when invocation IDs agree.
pub fn comparability(
    before: Option<&SourceIdentity>,
    after: Option<&SourceIdentity>,
    before_alive: bool,
    after_alive: bool,
) -> Comparability {
    if !before_alive || !after_alive {
        return Comparability::Disappeared;
    }
    let (Some(before), Some(after)) = (before, after) else {
        return Comparability::MissingIdentity;
    };
    if [&before.invocation_id, &after.invocation_id]
        .iter()
        .any(|id| id.as_ref().is_some_and(|id| id.is_empty() || id.len() > 64))
    {
        return Comparability::MissingIdentity;
    }
    if before.cgroup_path.is_empty()
        || after.cgroup_path.is_empty()
        || before.cgroup_path != after.cgroup_path
    {
        return Comparability::PathMismatch;
    }
    // A reboot ends every lifetime, regardless of recycled paths or stale IDs.
    if let (Some(a), Some(b)) = (&before.boot_id, &after.boot_id)
        && a != b
    {
        return Comparability::RestartDetected;
    }
    // Manager context or UID disagreement means the two samples were taken
    // under different authorities: never difference them.
    if let (Some(a), Some(b)) = (&before.manager_context, &after.manager_context)
        && a != b
    {
        return Comparability::ContextMismatch;
    }
    if let (Some(a), Some(b)) = (before.uid, after.uid)
        && a != b
    {
        return Comparability::ContextMismatch;
    }
    match (&before.invocation_id, &after.invocation_id) {
        // Equal invocation IDs are necessary but not sufficient: a
        // contradictory inode means the path was recycled underneath a
        // stale ID, so the samples cannot share one counter lifetime.
        (Some(a), Some(b)) if a == b => match (before.inode, after.inode) {
            (Some(x), Some(y)) if x != y => Comparability::ReplacementSuspected,
            _ => Comparability::Compatible,
        },
        (Some(_), Some(_)) => Comparability::RestartDetected,
        // One side (or neither) reports an invocation ID: no positive
        // continuity evidence. An inode change additionally suggests the
        // path was recycled for a new object.
        _ => match (before.inode, after.inode) {
            (Some(a), Some(b)) if a != b => Comparability::ReplacementSuspected,
            _ => Comparability::MissingIdentity,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(path: &str, invocation: Option<&str>, inode: Option<u64>) -> SourceIdentity {
        SourceIdentity::new(path, None, None, None, None, invocation, inode)
    }

    #[test]
    fn lifetime_matrix() {
        let a = id("/a/b.service", Some("inv-1"), Some(11));
        let b = id("/a/b.service", Some("inv-1"), Some(11));
        assert_eq!(
            comparability(Some(&a), Some(&b), true, true),
            Comparability::Compatible
        );
        let c = id("/a/other.service", Some("inv-1"), Some(11));
        assert_eq!(
            comparability(Some(&a), Some(&c), true, true),
            Comparability::PathMismatch
        );
        let d = id("/a/b.service", Some("inv-2"), Some(11));
        assert_eq!(
            comparability(Some(&a), Some(&d), true, true),
            Comparability::RestartDetected
        );
        let e = id("/a/b.service", None, Some(12));
        let f = id("/a/b.service", None, Some(13));
        assert_eq!(
            comparability(Some(&e), Some(&f), true, true),
            Comparability::ReplacementSuspected
        );
        // Same path, same inode, but no invocation evidence: not compatible.
        let g = id("/a/b.service", None, Some(11));
        let h = id("/a/b.service", None, Some(11));
        assert_eq!(
            comparability(Some(&g), Some(&h), true, true),
            Comparability::MissingIdentity
        );
        // One-sided invocation evidence: still no proof of continuity.
        assert_eq!(
            comparability(Some(&a), Some(&g), true, true),
            Comparability::MissingIdentity
        );
        // Equal invocation IDs with contradictory inodes: recycled path
        // under a stale ID, never one counter lifetime.
        let stale = id("/a/b.service", Some("inv-1"), Some(99));
        assert_eq!(
            comparability(Some(&a), Some(&stale), true, true),
            Comparability::ReplacementSuspected
        );
        // Boot change ends the lifetime even with agreeing invocation IDs.
        let booted = SourceIdentity::new(
            "/a/b.service",
            None,
            None,
            None,
            Some("boot-2"),
            Some("inv-1"),
            Some(11),
        );
        let unbooted = SourceIdentity::new(
            "/a/b.service",
            None,
            None,
            None,
            Some("boot-1"),
            Some("inv-1"),
            Some(11),
        );
        assert_eq!(
            comparability(Some(&unbooted), Some(&booted), true, true),
            Comparability::RestartDetected
        );
        // Manager context / UID disagreement is a context mismatch.
        let sys = SourceIdentity::new(
            "/a/b.service",
            None,
            Some("system"),
            Some(0),
            None,
            Some("inv-1"),
            Some(11),
        );
        let user = SourceIdentity::new(
            "/a/b.service",
            None,
            Some("user"),
            Some(1000),
            None,
            Some("inv-1"),
            Some(11),
        );
        assert_eq!(
            comparability(Some(&sys), Some(&user), true, true),
            Comparability::ContextMismatch
        );
        assert_eq!(
            comparability(None, Some(&b), true, true),
            Comparability::MissingIdentity
        );
        assert_eq!(
            comparability(Some(&a), Some(&b), true, false),
            Comparability::Disappeared
        );
    }

    #[test]
    fn strings_are_bounded() {
        let long = "x".repeat(2000);
        let id = SourceIdentity::new(&long, Some(&long), None, None, None, None, None);
        // Overlong values are rejected, never truncated into a collision.
        assert!(id.cgroup_path.is_empty());
        assert!(id.unit.is_none());
    }
}
