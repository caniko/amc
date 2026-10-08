# Changelog

## [Unreleased]

### Added

- Advance foreground preparation with durable drain barriers and once-only
  transfer into native scopes that preserve the caller's filesystem namespace.
- Finite, up-front-backed completion lanes and per-operation root worker
  ownership so existing work can finish while newer calls wait.
- Bounded incremental swap return with observed progress, explicit whole-device
  recovery/restoration, and native interruption and safe-wait qualification.
- Opt-in, bounded production observation for native services and scopes with
  aggregate host memory, swap, fault, and pressure context.
- Offline capture validation and an optional transient observer runner that
  exports its own CPU, memory, and IO accounting.
- Persistent per-user admission with private IPC, durable hard-memory
  reservations, verified native entry, and an optional Home Manager service.
- Installed `amc report` with Markdown/JSON output and a booted-Linux diagnostic
  smoke test, verified on Ubuntu 24.04.5.
- Optional systemd-owned execution deadlines for disposable admitted jobs,
  independent of the queue wait timeout.
- Standalone systemd units, a small admission policy and an operational
  workstation guide covering useful work, upgrade, disable and removal.

### Changed

- Observation summaries now report per-field interval endpoints, attempted and
  persisted samples, storage durability, and collection timing.
- Workstation delivery prioritizes foreground stability and bounded useful
  progress; CS2 recording and frame-time comparisons remain optional diagnostics.

### Fixed

- Keep finite completion calls available after rejected enqueue attempts, bound
  aggregate admission snapshots, and retain existing obligations on rejected growth.
- Include parents that can launch future completion children in preparation drains
  and require a ready window long enough for the native transfer.
- Keep burst backfill available when draining cannot resolve an aged request's
  swap-return, swap-headroom, completion-escrow, or native-ancestor wait.
- Preserve prepared and completion native claims during whole-device recovery;
  report empty page-return selection and unfinished multi-device return as incomplete.
- Allow finite root services alongside operation-serial-owned root pools.
- Preserve literal arguments, streams, exit status, and startup denial for waited
  native/admitted launches from private PID and remapped user namespaces.
- Preserve verified pre-change counter and pressure evidence across restarts
  without combining values from different workload lifetimes.
- Mark final counters unavailable when sample persistence fails or the final
  target identity cannot be confirmed.
- Bind admission cancellation to the submitting process and retain reservations
  across client death, coordinator restart, and unavailable native evidence.
- Refuse malformed boot identities and damaged or missing initialized admission
  ledgers instead of forgetting workload reservations.
- Preserve literal workload arguments when submitting transient services.
- Stop manager-option validation at the workload argv separator and reject
  caller properties that override managed execution's run-once lifecycle.
- Show target measurement coverage, collection flags, and event intervals in
  capture reports; safely render JSON-escaped lone Unicode surrogates.
- Keep the executable wrapper out of marker-only Nix check outputs and handle
  unavailable host cgroup trees explicitly in sandboxed runner tests.
