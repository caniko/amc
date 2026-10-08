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
charged until its descendants are empty and all potential execution owners have
terminated or positively finished their finite operations. When preparations
are configured, each root worker supplies a distinct nonzero `operation` serial.
`release_pool` settles only that root peer's matching serial, not native capacity.
Nested workers and other peers keep their ownership; a lost release retains
backing, and a newer serial cannot retry with the old worker's admission rights.

Native observations are conservative: unknown or replaced cgroups retain
capacity, changed granted ceilings inhibit new admission, swap never extends
RAM headroom, and existing resident commitments are deliberately double-counted.
These guarantees cover cooperative enrolled execution, not arbitrary services
outside the configured domains or a privileged caller changing enforcement.

`amc admission host-status` returns versioned JSON; unprivileged callers see only
their reservation identities. User callers have no capacity-release authority.
Root execution owners may settle finite operations; users cannot release a
running grant by cancelling a socket or a preparation.

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

## Advance foreground preparation

Optional `preparations` profiles specify a non-root slice domain, native
memory/swap ceilings, `drain_domains`, a bounded `wait_ms` and a `ready_ms` of
15–60 seconds, covering the bounded host-native registration and consume path.
The flake and package export `admissionPreparationVersion = 2`.

Waited `amc exec` and `amc admission exec` calls from a private PID or remapped
user namespace use a 64 MiB, zero-swap host-native runner. The runner performs
the ordinary admission and startup checks in the user manager's PID/UID view;
the namespace-local caller keeps stdin/stdout/stderr and waits for its result.
This managed-execution path requires the host user-manager session bus and
host-visible executable/working-directory paths. `amc prepare` instead registers
the waiting payload in a host-native scope and preserves its namespaces.

```sh
amc prepare --profile game -- game-command args
```

An intent immediately closes ordinary and burst admission. Existing grants
continue; the broker waits for the selected finite operations and their admitted
completion children to finish. Intentions are FIFO, survive broker restart, and
expire or cancel without revoking running work. A ready intent owns a real
host/native-ancestor claim. The hidden entry helper authenticates an atomic,
once-only transfer into a native scope before executing the payload. Replaying a
lost reply is permitted only for that same native peer. After transfer, only
observed cgroup cleanup can release the claim, including surviving descendants.

Prepared scopes preserve the caller's environment, working directory, stdio and
filesystem, user and PID namespaces. A bounded host-native helper, launched by
the local user manager, authenticates the root broker and registers the waiting
process's kernel-authenticated host PID in the ready-backed scope. The broker
rechecks the nominated PID/start-time and host UID before the once-only consume;
only then does the waiting process exec its payload in place. User namespace
overflow UIDs never authenticate the root broker. A private per-attempt runtime
socket and token bind the acknowledgement to the waiting process; helper loss,
caller cancellation and failed registration never execute the payload.

This permits warm Steam/pressure-vessel launch commands
whose paths exist only inside their runtime. `--payload-env NAME=VALUE` applies
loader and GameMode settings after admission, so the waiting helper does not
quiesce the old work it needs to drain. Prepared execution needs a reachable
local user manager and a writable, host-shared `XDG_RUNTIME_DIR` rendezvous.
Helper creation and scope registration use explicit `StartTransientUnit` calls
through the host-shared runtime directory's session bus. Automatic private-peer
manager connections cannot observe the manager's PID from a private PID namespace.
The helper has a 64 MiB memory cap, zero swap and a one-hour runtime bound; its
literal argv contains only rendezvous/broker paths and the profile, with environment
expansion disabled. The payload command and loader settings stay with the caller.
The helper uses the host cgroup-v2 view, so the payload need not see host PIDs.
Namespace configurations that cannot register their native scope fail before
the payload; they do not fall back to uncontained execution.

Domains can give small parents finite `continuation` contracts: allowed child
domains, parent/child ceilings, and at most 64 distinct calls. Admission backs
one completion lane per child domain in advance, at both host and native
ancestors. A child transfers that lane's backing and propagates the authenticated
capability to further calls. Same-domain children serialize; separate domains can
finish nested operations. Already-queued children retain their obligation after
parent exit. Call/UID/domain/ceiling bounds continue to apply during draining.
Completion rights do not grow on policy reload or authorize arbitrary new work.
Root Nix clients bind these rights to the original socket peer's kernel identity.

## Swap-return priority and recovery

`reserve_swap_return = true` charges observed nonresident host swap demand not
covered by spare resident capacity inside native grants before new memory growth.
Resident swap cache is already in RAM and in `memory.current`; counting it again
would reserve that RAM twice. Overlapping native boundaries cannot credit the
same pages twice. Unknown or inconsistent return accounting blocks admission.
The capability is
`lib.swapReturnReservationVersion = 2` (also exported on the package).

A root-only `swap_recovery` policy selects a finite maintenance unit, helper
ceiling, nonoverlapping `page_cgroups`, `batch_bytes` (4 KiB–16 MiB), and optional
device `targets`. The default recovery faults bounded private readable page
ranges through a pinned `/proc/<pid>/mem` descriptor, with process identity and
leaf/ancestor headroom checks. It never writes target memory or reports payload
bytes. A batch is backed before reads. Pinned native pagemap observations must
prove every requested page resident after reading; the broker independently
checks the range before settling its claim. Occupied swap slots and resident
`memory.stat` swap cache are separate telemetry. Read faults can bring a page
back into RAM while Linux retains its swap slot: slot deletion is not RAM return.

```sh
amc recover-swap                 # bounded incremental return, devices stay on
amc recover-swap --whole-device  # explicit conservative swapoff/restore
amc recover-swap --restore       # restore declared devices after interruption
```

Recovery can enter below the ordinary free-swap floor, but retains the host RAM
reserve, pressure gates, helper backing and native ancestor checks. Bounded
batches can start even when the entire return debt cannot fit. Other admission
waits while a campaign owns recovery; cancellation, read failure and broker
restart retain the claim until native cleanup. Exit 75 means waiting, stalled or
incomplete return, including partial progress or unreadable pages. Exit 0 for
page recovery requires zero remaining nonresident return demand in the selected
subtrees. The hosted receipt additionally proves the entire fixture mapping is
resident before the target's own probe and that its bytes remain intact. Retained
resident swap slots are allowed and recorded. Whole-device recovery needs backing
for the full device and all affected native domains, and restores it before
reporting success.
Consumers should install `--restore` as maintenance-unit `ExecStopPost` and keep
device restoration separate from admission release.

The shared-admission VM contains drain/restart, warm nested launch,
namespace-preservation, surviving-descendant, real pageout/return, safe-wait and
interrupted-campaign scenarios. These are qualification gates, not evidence of
live Steam/Proton containment or successful recovery on a deployed workstation.
