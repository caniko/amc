# Changelog

## [Unreleased]

### Added

- Opt-in, bounded production observation for native services and scopes with
  aggregate host memory, swap, fault, and pressure context.
- Offline capture validation and an optional transient observer runner that
  exports its own CPU, memory, and IO accounting.
- Persistent per-user admission with private IPC, durable hard-memory
  reservations, verified native entry, and an optional Home Manager service.
- Installed `amc report` with Markdown/JSON output and a booted-Linux diagnostic
  smoke test, verified on Ubuntu 24.04.5.

### Changed

- Observation summaries now report per-field interval endpoints, attempted and
  persisted samples, storage durability, and collection timing.

### Fixed

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
