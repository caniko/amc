# Changelog

## [Unreleased]

### Added

- Opt-in, bounded production observation for native services and scopes with
  aggregate host memory, swap, fault, and pressure context.
- Offline capture validation and an optional transient observer runner that
  exports its own CPU, memory, and IO accounting.
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
- Stop manager-option validation at the workload argv separator and reject
  caller properties that override managed execution's run-once lifecycle.
- Show target measurement coverage, collection flags, and event intervals in
  capture reports; safely render JSON-escaped lone Unicode surrogates.
- Keep the executable wrapper out of marker-only Nix check outputs and handle
  unavailable host cgroup trees explicitly in sandboxed runner tests.
