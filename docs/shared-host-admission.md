# Shared host reservation broker

`amc admission host-serve --policy FILE` runs a root-owned broker. Its default
socket is `/run/amc-host/admission.sock`; durable state is `/var/lib/amc-host`.
The optional `nixosModules.host-admission` module installs the service.

The version 1 host policy defines `budget_bytes`, `reserve_bytes`,
`swap_reserve_bytes`, full memory/I/O PSI thresholds, recovery/aging intervals,
a bounded queue and enrolled domains. Every domain specifies its name, peer UID,
cgroup subtree, maximum allowed memory/swap ceilings and `fair_share_bytes`.
Domains cannot overlap. One UID's domain shares must agree.

Ceilings are read from native cgroups, never trusted from a user payload. The
broker serializes and persists grants before replying. Soft shares govern
priority while idle shares remain borrowable; aging eventually stops backfill
behind a waiting large request. RAM and swap reservations are independent.

Private coordinators can use `--host-socket` (Home Manager `hostSocket`). Their
entry helper acquires capacity and the coordinator verifies its durable host
grant before permitting execution. Native lifecycle checks still require
`Restart=no`, `KillMode=control-group`, `OOMPolicy=kill` and matching ceilings.

`acquire_pool` is reserved for root execution owners such as Nix. It enrolls a
policy-defined, hard-bounded aggregate pool once across multiple handler PIDs.
Socket disconnect, timeout and restart cannot release a grant. The pool remains
charged until its descendants and all potential execution owners terminate.

Native observations are conservative: unknown or replaced cgroups retain
capacity, changed granted ceilings inhibit new admission, swap never extends
RAM headroom, and existing resident commitments are deliberately double-counted.
These guarantees cover cooperative enrolled execution, not arbitrary services
outside the configured domains or a privileged caller changing enforcement.

`amc admission host-status` returns versioned JSON; unprivileged callers see only
their reservation identities. There is no client-controlled release API.

Regression coverage includes native empty/inode evidence, duplicate requests,
cross-user races, borrowed shares, aging, policy reduction, missing observations,
swap growth, shared ancestors and durable-state restart/corruption. The
`nixosTests.<system>.shared-admission` gate additionally exercises real user-manager
entry, overlapping users, broker restart, automatic resumption, changed native
enforcement, entry attempts omitting the host handshake, cancellation, root pool
owner retention and per-user status isolation. Native VM/package qualification
is required before deployment.

## Domain pressure policy

Host domains may explicitly set `io_pressure = "diagnostic"` and
`min_available_bytes` for evaluation workloads. Missing fields preserve the
original policy: enforced host I/O PSI and no additional RAM floor. The flake
exports `lib.hostDomainPressureVersion = 1` for consumers that need this schema.

Diagnostic I/O never bypasses memory/swap observations, the domain RAM floor,
ceiling-backed host/ancestor accounting, fairness, native identity validation or
cleanup. Missing or malformed I/O telemetry stops only enforced domains. Each
domain retains its own recovery window; an aged pressure-inhibited domain does
not block healthy peers, while capacity-inhibited requests retain aging priority.
