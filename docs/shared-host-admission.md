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

## Short-call burst admission

An optional `burst` host policy adds a separate scheduling allowance above
`budget_bytes`. It specifies `budget_bytes`, `max_job_bytes`, `max_running`,
`max_runtime_ms` (1–30 seconds), and a per-UID `min_interval_ms` start interval
longer than the runtime plus one-second cleanup grace. Only non-root domains
explicitly marked `burst = true` may use it, with zero swap. Policies without
these fields keep their existing admission and serialized contract shape.

The matching private contract sets `burst = true`, `runtime_max_sec`, and a
`burst_budget_bytes` private allowance. It must use a dedicated finite slice;
burst entry always requires the shared host broker. Callers may shrink their
hard memory ceiling with `--max-ram-usage`; they cannot enlarge the contract:

```sh
amc exec --burst --max-ram-usage 512MiB --runtime-max-sec 5 -- command args
```

`amc exec` routes admitted calls through the existing persistent coordinator;
the default burst contract is `tool-burst`. `amc admission exec --contract
tool-burst --burst ...` is the equivalent explicit route. Integral decimal
KB/MB/GB and binary KiB/MiB/GiB sizes are accepted, rounding down to a 4096-byte
boundary. Legacy `amc exec --slice ... --memory-max ... --memory-swap-max ...
--runtime-max-sec ...` retains isolation-only behavior and cannot be mixed with
burst options. The package exports `nativeExecVersion = 1`; the flake exports
`lib.admissionBurstVersion = 1` for capability-gated consumers.

The root broker independently reads the kernel ceilings and verifies the actual
user manager's `RuntimeMaxUSec`, zero randomized extension, `Restart=no`,
control-group killing, `OOMPolicy=kill`, one-second stop timeout and final
SIGKILL. The native timer includes the host-admission handshake. The requested
runtime is a hard deadline, not a claim that the command will finish in time.
Host RAM/swap reserves, pressure recovery, all ancestor ceilings and full live
commitments still apply. Daemon-owned work remains independently accounted.

An aged normal job blocked by existing normal commitments may allow burst
backfill. When draining bursts would let that normal job fit, further burst
backfill stops to give it a quiet window. Burst concurrency, aggregate capacity
and persisted per-UID start cooldowns prevent repeated short calls from turning
the allowance into a sustained extra workload lane.

Granted bursts stay charged until native descendant cleanup is confirmed,
including after deadlines, disconnected clients, broker restart and policy
disablement. New model turns have no release authority. Host-status JSON exposes
`budget_bytes`, `burst_budget_bytes`, `burst_committed_bytes`, and each grant's
verified ceiling/class/runtime. The native shared-admission VM includes above-
budget execution, restart, cross-user bursts, ignored-SIGTERM descendants and
cooldown deferral. Passing Rust fixtures alone does not qualify native rollout.
