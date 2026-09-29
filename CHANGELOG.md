# Changelog

## [Unreleased]

### Added

- Opt-in, bounded production observation for native services and scopes with
  aggregate host memory, swap, fault, and pressure context.
- Offline capture validation and an optional transient observer runner that
  exports its own CPU, memory, and IO accounting.

### Changed

- Observation summaries now report per-field interval endpoints, attempted and
  persisted samples, storage durability, and collection timing.

### Fixed

- Preserve verified pre-change counter and pressure evidence across restarts
  without combining values from different workload lifetimes.
- Mark final counters unavailable when sample persistence fails or the final
  target identity cannot be confirmed.
