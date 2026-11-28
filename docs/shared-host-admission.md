# Shared host reservation broker

`amc admission host-serve --policy FILE` runs a root-owned broker with socket
`/run/amc-host/admission.sock` by default and durable state at `/var/lib/amc-host`.
The optional `nixosModules.host-admission` module installs the service.

Host policy version 1 sets `budget_bytes`, `reserve_bytes`, `swap_reserve_bytes`,
full memory/I/O PSI thresholds, recovery/aging intervals, a bounded queue and
enrolled domains. Nonoverlapping domains specify name, peer UID, cgroup subtree,
maximum memory/swap ceilings and `fair_share_bytes`. Shares must agree per UID.

The broker reads ceilings from native cgroups, not user payloads. It serializes
and persists grants before replying. Soft shares set priority; idle shares are
borrowable. Aging eventually stops backfill behind large waiting requests.
RAM and swap reservations are independent.

Private coordinators can use `--host-socket` (Home Manager `hostSocket`). The
entry helper acquires capacity; the coordinator verifies the durable host
grant before execution. Native lifecycle checks still require
`Restart=no`, `KillMode=control-group`, `OOMPolicy=kill` and matching ceilings.

Only root execution owners such as Nix can use `acquire_pool`. It enrolls a
policy-defined, hard-bounded aggregate pool once across handler PIDs. Socket
disconnect, timeout and restart don't release grants. A pool stays charged until
no descendants remain and every potential execution owner terminates or
demonstrably finishes its finite operation. When preparations are configured,
each root worker supplies a distinct nonzero `operation` serial.
`release_pool` settles only that root peer's matching serial, not native capacity.
Persisted PID/start, pool inode and operation authenticate release despite domain
removal or ceiling reduction, so a live daemon PID cannot trap a completed
Worker on policy change. Nested workers and other peers keep their ownership.
Lost release retains backing; newer serials cannot retry with old worker rights.

Unknown or replaced cgroups retain capacity. Changed granted ceilings inhibit
new admission. Swap never extends RAM headroom, and accounting deliberately
double-counts resident commitments. Guarantees cover cooperative enrolled work,
not services outside configured domains or privileged changes to enforcement.

`amc admission host-status` returns versioned JSON; unprivileged callers see only
their own reservation identities. Root execution owners may settle finite
operations. Users cannot release capacity, even by cancelling a socket or
preparation for a running grant.

Regression tests cover native empty/inode evidence, duplicate requests,
cross-user races, borrowed shares, aging, policy reduction, missing observations,
swap growth, shared ancestors and durable-state restart/corruption.
`nixosTests.<system>.shared-admission` adds real user-manager entry, overlapping
users, broker restart, automatic resumption, changed native enforcement, attempts
to enter without the host handshake, cancellation, root pool owner retention and
per-user status isolation. Deploy only after native VM/package qualification.

## Domain pressure policy

For evaluation workloads, host domains may set `io_pressure = "diagnostic"` and
`min_available_bytes`. Omitting both fields preserves the original policy
defaults: enforce host I/O PSI with no added RAM floor.
The flake exports `lib.hostDomainPressureVersion = 1` for schema detection.

Diagnostic I/O preserves memory/swap observations, the domain RAM floor,
ceiling-backed host/ancestor accounting, fairness, native identity checks and
cleanup. Missing or malformed I/O telemetry stops only enforced domains.
Recovery windows are per domain: aged pressure-inhibited domains don't block
healthy peers; capacity-inhibited requests keep aging priority.

## Short-call burst admission

Optional host policy `burst` adds a scheduling allowance above `budget_bytes`.
It sets `budget_bytes`, `max_job_bytes`, `max_running`, `max_runtime_ms`
(1–30 seconds) and a per-UID `min_interval_ms` between starts, longer than
runtime plus one-second cleanup grace. Only non-root domains explicitly marked
`burst = true` may use the allowance, and burst calls must use zero swap. Omit
the fields to preserve existing admission behavior and serialized contract shape.

The matching private contract sets `burst = true`, `runtime_max_sec` and the
private `burst_budget_bytes` allowance, and must use a dedicated finite slice.
Burst entry requires the shared host broker. `--max-ram-usage` may lower this
call's hard memory ceiling; it cannot enlarge or change the contract:

```sh
amc exec --burst --max-ram-usage 512MiB --runtime-max-sec 5 -- command args
```

`amc exec` routes admitted calls through the existing persistent coordinator,
using `tool-burst` by default for bursts. The equivalent explicit route is
`amc admission exec --contract tool-burst --burst ...`. Integral decimal
KB/MB/GB and binary KiB/MiB/GiB sizes round down to a 4096-byte boundary. Legacy
`amc exec --slice ... --memory-max ... --memory-swap-max ... --runtime-max-sec ...`
only isolates and cannot mix with burst options. The package exports
`nativeExecVersion = 1`; the flake exports `lib.admissionBurstVersion = 1` for
capability-gated consumers.

The root broker independently reads kernel ceilings and checks the actual
user manager for `RuntimeMaxUSec`, zero randomized extension, `Restart=no`,
control-group killing, `OOMPolicy=kill`, one-second stop timeout and final
SIGKILL. The native timer includes the host handshake. Runtime is a hard
deadline, not a completion guarantee. At most eight concurrent background
queries refresh manager evidence outside the broker, prioritizing live grants
over waiters. Evidence binds exact identity and expires after one second.
Missing evidence delays admission; the broker still rechecks kernel limits
directly. Manager latency cannot hold ordinary broker RPCs. Host RAM/swap
reserves, pressure recovery, all ancestor ceilings and full live commitments
still apply. Daemon-owned work stays independently accounted.

An aged normal job blocked by normal commitments may allow burst backfill.
Backfill stops when draining bursts would let the job fit. Concurrency,
aggregate capacity and persisted per-UID start cooldowns prevent bursts from
becoming a sustained extra workload lane.

Bursts stay charged until verified native descendant cleanup despite deadlines,
disconnects, broker restart or policy disablement. New model turns have no
release authority. Host-status JSON exposes `budget_bytes`, `burst_budget_bytes`,
`burst_committed_bytes` and each grant's verified ceiling/class/runtime.
The native shared-admission VM covers above-budget execution, restart, cross-user
bursts, descendants ignoring SIGTERM and cooldown deferral. Passing Rust
fixtures alone does not qualify native rollout.

## Protected, policy-owned small-call backing

Unlike bursts, domain `protected_pool` is optional fixed backing inside
`budget_bytes`, even without live children. The matching private contract uses
an exclusive sibling slice and the same `protected_pool` bounds:
`source_contract`, `memory_bytes`, `max_job_bytes`, `max_running`,
`runtime_max_sec`, `wait_max_sec`, `queue_limit`, `cpu_quota_usec` (per 100 ms),
and `tasks_max`. The envelope must exceed concurrent per-call ceilings for
helper/cache headroom. Require zero swap, finite aggregate CPU/PIDs and a native
service deadline with one-second final-kill grace. Enroll at most one pool per
UID, with at most eight live protected calls host-wide.

```sh
amc admission exec --contract tool --protected-pool \
  --max-ram-usage 128MiB --runtime-max-sec 15 -- command args
```

`enqueue_bounded` selects the policy's protected contract only if both declared
limits fit. Sizing alone and legacy requests don't opt in; larger initial
declarations keep their ordinary contract. A full selected pool never
automatically promotes, resubmits or replays the command. Choose before an outer
tool shell waits for bulk admission. The package and flake export
`admissionProtectedPoolVersion = 1`; gate both the CLI flag and policy fields on it.

Root and private accounting charge backing once, not again per child. Separate
bounded queues and native deadline-observation workers isolate protected capacity
from bulk/burst traffic. Projections include residual cache in native aggregate
backing and reuse the full-envelope ancestry check. Pressure/unknown observations,
supervision, foreground preparation/drain and swap-recovery barriers still
inhibit execution. This cannot guarantee availability under physical memory
exhaustion or arbitrary uncooperative workloads.

Backing survives idle and occupied restarts. Native slots stay charged until
verified cleanup, not disconnect or expiry. Reduction or retirement keeps old
backing until the aggregate is absent or verified empty with zero charge. An
offline user's absent slice stays pre-accounted without blocking admission
globally; an existing unbounded slice fails closed. Drain before changing pool
domain identity. Keep ledgers on policy rollback: older binaries cannot safely
parse the new state.

Offline tests cover queue/budget isolation, bounded selection, restart snapshots,
unknown cleanup, reduced backing and native evidence/pressure waits. Require a
passing `nixosTests.<system>.protected-pool` (real user managers and hard failures)
before enabling deployment. Source support and passing Rust tests alone do not
establish native qualification.

## Advance foreground preparation

Optional `preparations` profiles specify a non-root slice domain, native
memory/swap ceilings, `drain_domains`, bounded `wait_ms` and a `ready_ms` of
15–60 seconds for bounded host-native registration and consume. The path must
match the complete dash-derived slice hierarchy below the UID's user-manager
root: `app-game.slice` for UID 1000 requires
`/user.slice/user-1000.slice/user@1000.service/app.slice/app-game.slice`.
This user-bus launch path rejects host-root slices and `app.slice/game.slice`
before creating an intent.
The flake and package export `admissionPreparationVersion = 2`.

Waited `amc exec` and `amc admission exec` calls from private PID or remapped
user namespaces use a 64 MiB, zero-swap host-native runner. It performs ordinary
admission and startup checks in the user manager's PID/UID view. The caller
keeps stdin/stdout/stderr and waits in its namespace for the runner's result.
This path needs the host user-manager session bus and host-visible
executable/working-directory paths. `amc prepare` instead registers the waiting
payload in a host-native scope and preserves its namespaces.

Shared namespace execution additionally requires `namespace_runner_bytes`
(64 MiB–1 GiB per enrolled non-root UID). Each user's host runners and preparation
helpers share the `app-amchostrunner.slice` created by the host module at that
exact aggregate RAM ceiling and zero swap. Each service keeps its 64 MiB cap.
Policy validation requires the sum of these aggregate envelopes to fit inside
`reserve_bytes`. The ledger keeps prior UIDs and ceilings through restart,
removal and allowance cuts until the aggregate is verified empty. Startup
discovers surviving aggregate slices from older snapshots too. If the sum of
retained and current allowances exceeds a reduced host reserve, new admission
waits for existing native work to finish.

Native projections back these allowances in finite ancestors even while runners
wait. The runner authenticates its allowance with the broker before submitting
the inner shared job. This static maintenance backing prevents preparations from
waiting on runners that are themselves waiting for admission. Each `Prepare`
independently checks exact aggregate/leaf placement, native caps and ancestor
backing before creating a barrier. A separate handshake grants no preparation
authority to arbitrary same-UID processes. Host status reports
`namespace_runner_reserved_bytes`; consumers can require
`lib.admissionNamespaceRunnerVersion = 2` for namespace runners and concurrent
preparation helpers (version 1 covered only runners). Shared namespace execution
without an allowance fails before payload submission.

Preparation profiles require a backed runner allowance; zero is valid when
preparations are disabled. Profile names pass as one literal option value, even
when they start with a hyphen.

```sh
amc prepare --profile game -- game-command args
```

An intent immediately closes ordinary and burst admission. Existing grants
continue while the broker waits for selected finite operations and their
admitted completion children to finish. Intents are FIFO, survive broker restart and can expire
or cancel without revoking running work. Pending intents bind the authenticated
host helper PID and start time. Verified helper death (including SIGKILL or a
zombie) removes Draining/Ready barriers and backing at the next tick or restart.
Poll, consume and periodic/restart reconciliation recheck helper UID,
aggregate/leaf placement, limits and ancestor backing. Missing or changed
evidence discards pending intents; replay discards legacy ownerless ones.

Active native claims stay charged after helper loss. A ready intent holds a
host/native-ancestor claim. Before payload execution, the hidden entry helper
authenticates its atomic, once-only transfer into a native scope. A lost reply
may be replayed only to that same native peer. Once transferred, only observed
cgroup cleanup, including surviving descendants, releases the claim.

Prepared scopes preserve environment, working directory, stdio and filesystem,
user and PID namespaces. The local user manager launches a bounded host-native
helper that authenticates the root broker and registers the waiting process's
kernel-authenticated host PID in the ready-backed scope. The broker rechecks the
nominated PID/start-time and host UID before the once-only consume; only then
does the waiting process exec in place. User namespace overflow UIDs never
authenticate the root broker. A private per-attempt runtime socket and token bind
the acknowledgement to the waiting process. The payload never runs on helper
loss, caller cancellation or failed registration.

Warm Steam/pressure-vessel launches can use runtime-only paths.
`--payload-env NAME=VALUE` applies loader and GameMode settings after admission,
so the waiting helper doesn't quiesce work it must drain. Preparation needs a
reachable local user manager and writable, host-shared `XDG_RUNTIME_DIR`
rendezvous. Helper creation and scope registration make explicit
`StartTransientUnit` calls through that directory's session bus. Automatic
private-peer manager connections cannot see the manager's PID from a private
PID namespace. The helper has a 64 MiB memory cap, zero swap and a one-hour runtime
bound. Its literal argv carries only rendezvous/broker paths and the profile,
without environment expansion. Payload command and loader settings stay with the
caller. The helper uses the host cgroup-v2 view, so the payload needn't see host
PIDs. Failed native scope registration stops before execution, with no
uncontained fallback.

Domains can give small parents finite `continuation` contracts: allowed child
domains, parent/child ceilings and up to 64 distinct calls. Admission pre-backs
one completion lane per child domain both at the host and in the native
ancestors. A child transfers lane backing and passes the authenticated
capability to further calls. Same-domain children serialize; separate domains
can finish nested operations. Queued child obligations survive parent exit.
Call/UID/domain/ceiling bounds still apply during draining. Completion rights
neither grow on reload nor authorize arbitrary new work. Root Nix clients bind
them to the original socket peer's kernel identity.

An optional `envelopes` map reduces `memory_bytes`/`swap_bytes` for named child
domains. Each override must name an allowed domain and fit both that native
domain and the default child ceiling. Escrow sums lanes; their limits persist with the parent across reloads.
Require `admissionContinuationEnvelopesVersion >= 1` before emitting overrides.
`amc admission host-policy-check --policy FILE` validates the emitted policy
without starting a broker. Root completion domains use fixed native pool
requests: envelopes must equal the domain's RAM and swap ceilings. Smaller
overrides apply only to individually enforced non-root children.

## Swap-return priority and recovery

Before new memory growth, `reserve_swap_return = true` charges observed
nonresident host swap demand beyond spare resident capacity inside native grants.
Resident swap cache already counts in RAM and `memory.current`; reserving it
again would double-count. Overlapping native boundaries cannot credit pages
twice. Unknown or inconsistent return accounting blocks admission. The capability
is `lib.swapReturnReservationVersion = 2` (also exported on the package).

Root-only `swap_recovery` selects a finite maintenance unit, helper ceiling,
nonoverlapping `page_cgroups`, `batch_bytes` (4 KiB–16 MiB) and optional device
`targets`. Default recovery faults bounded, private, readable page ranges through
a pinned `/proc/<pid>/mem` descriptor, checking process identity and leaf/ancestor
headroom. It never writes target memory or reports payload bytes. Reads require
batch backing first. Pinned native pagemap must prove every requested page
resident after reads; the broker independently checks the range before settling
the claim. Occupied slots and resident `memory.stat` swap cache are separate
telemetry. Linux can retain a slot after a read fault returns its page to RAM:
slot deletion is not RAM return.

Linux retains the original swap-entry memcg charge after process migration.
Even frozen cgroups can be migrated. For an offline memcg, direct faults can
use the target mm, but `__read_swap_cache_async` passes a null mm and falls back
to the reader's memcg: the finite recovery helper for remote `/proc/PID/mem`.
A swapped PTE doesn't expose its charge owner to this userspace API, and one mm
can mix page origins. Each batch therefore checks every online cgroup with
occupied slots and both current target and reader fallbacks against their
native ancestors and commitments. Every possible owner must back the entire
batch or prove all its slots are already in RAM with stable, exact cache
accounting. Fully backed owners need no cache-residency credit, so changing
cache counters can't invalidate their proof. Tight owners need the exact
no-new-RAM exception; malformed slot counts or unproven backing fail closed.
Recovery may wait for an unrelated charged domain to gain headroom; the
destination grant can't cover another owner. The native VM requires separate
proofs of online original charging, frozen migration, offlining fallback charge
attribution and all headroom denials with zero OOM events. See Linux
[`mem_cgroup_swapin_charge_folio`](https://github.com/gregkh/linux/blob/v6.18.48/mm/memcontrol.c#L4779-L4799)
and [cgroup memory ownership](https://docs.kernel.org/admin-guide/cgroup-v2.html#memory-ownership).
See the [swap-cache call site](https://github.com/gregkh/linux/blob/v6.18.48/mm/swap_state.c#L480):
target placement alone cannot identify the fallback owner. Returned pages must
fit the helper's remaining native resident allowance as well as its bounded
working memory; later batches wait when it is full. Charge-owner inventory
allows up to 65,536 groups, depth 256 and 20 seconds. Incomplete inventory
rejects acquisition without faulting pages. Root recovery acquisition RPCs allow
90 seconds for bounded scan, feasibility and full replay, so complete wide
scans can outlast the ordinary two-second program-call deadline. Device inventory
also has a 20-second deadline within that maintenance RPC budget.

Page and device inventories and complete live replays run in one bounded
background task outside the accept loop, keeping ordinary status, preparation
and acquisition RPCs responsive. Scan clones cannot write the ledger or grant
leases. The broker rejects stale backing, replaced helpers, disconnected peers
and results older than one second, then repeats native identity, host capacity
and local fallback checks before persisting the same lease. Abandoned scans
cannot transfer changed ownership or demand. Inventory backing includes granted
native obligations and Ready/continuation escrow; ungranted queue churn cannot
discard a valid scan.

New page leases record slot occupancy without the optional aggregate cache delta:
`before_return_bytes` and `return_demand_reduction_bytes` are null. Batch authority
still requires all-swapped PTEs before admission and fully resident PTEs at
settlement. Historical leases keep exact cache-delta accounting. Whole-device
feasibility, swap-return reservation and final zero-demand completion still need
exact accounting. Missing telemetry never proves completion.

Page and whole-device replay verify the complete scanned cgroup frontier and
recalculate headroom from live limits, resident bytes and uncovered swap demand,
even for groups previously at zero demand. Page replay rechecks every possible
original charge owner and target and reader fallback boundaries. Replay has a
20-second deadline and finishes before the worker timestamps results. The main
loop rechecks freshness after final native target/helper and capacity validation,
before persisting the exact batch. Wide hierarchies cannot block the accept loop;
incomplete replay never uses scan-time native demand.

### Kernel page-return guard (version 1)

Incremental page return requires package capability `pageReturnKernelGuardVersion = 1`
and a booted kernel with both patches in `nix/kernel/` (Linux 6.18.48 baseline).
The host-admission module adds them for configured page-return subtrees;
standalone consumers can import `nixosModules.page-return-guard`. Package
capability alone cannot prove running kernel support. Missing or unproven native
guards return status 75 before any recovery read.

Root opens `/proc/<pid>/amc_mem`, a read-only, ptrace-authorized proc-mem
descriptor requiring `CAP_SYS_ADMIN` in the initial user namespace as well.
Open serializes with cgroup migration and pins the **same mm served by its
reads** and the target's memory cgroup. Kernel fdinfo reports guard version,
opaque mm cookie and memcg inode. Helper and target descriptors stay open
through inventory, grant, remote reads and broker-verified settlement. The
broker independently opens current guarded mms and authenticates matching kernel
cookies in the native helper's descriptor table; there is no request-body guard
assertion. Both cookies persist in the lease and must match at settlement: exec
or last-close/reopen cannot recycle a grant into a replacement mm. Descriptor
inventories have count, size and time bounds.

Guarded mm sharers get `EBUSY` on memory-controller migration, with no
`CLONE_VM | CLONE_INTO_CGROUP` bypass. The pin prevents removal or controller/type
changes for the cgroup and its ancestry. The fallback memcg stays pinned if the
mm owner dies and an existing cross-cgroup sharer survives. A private fork/exec
mm starts unguarded; obsolete proc descriptors cannot reach the replacement.
Closing the last guarded FD clears the pin with RCU-safe CSS lifetime management.
Duplication, helper death and broker restart keep normal kernel FD semantics.

Guarded swap-in uses order-0 faults without readahead, so backed batches cannot
allocate unreserved larger folios or neighboring cache pages. Kernel fdinfo
counts successful direct/cache swap-in pages. The disposable VM forces migration
**after grant and before read** on regular-swap cache and zram direct paths,
covering offlined original owners, reader migration, duplicated FDs, broker
restart and helper-loss cleanup. Receipts require exact per-page `kpagecgroup`
attribution, complete batch residency and zero OOM counters. A separate native
lifetime fixture checks pre-existing cross-cgroup mm sharers, conflicting guard
acquisition, `clone3` placement, private-fork initialization, owner death, memcg
offlining/controller changes, exec and last-FD cleanup.

The guard module selects the Linux 6.18 or 7.2 bounded-fault patch and rejects
unsupported series at evaluation. Hosted qualification runs the full
shared-admission VM on the pinned Nixpkgs kernel and Canix's exact pinned
CachyOS 7.2.8 source with ThinLTO and baseline CPU settings. The latter covers
the changed 7.2 swap-cache allocator and records the running kernel series. Atlas's
Zen4-optimized build and runtime remain consumer qualification gates.

Source anchors for this extension are Linux 6.18.48 `fs/proc/base.c` (`mem_open`,
`mem_rw`, `mem_release`), `kernel/cgroup/cgroup.c` (`cgroup_migrate_execute`,
`cgroup_can_fork`, controller/type writes and `cgroup_destroy_locked`),
`mm/memcontrol.c` (`get_mem_cgroup_from_mm`, `mem_cgroup_swapin_charge_folio`),
`mm/memory.c` (`alloc_swap_folio`) and `mm/swap_state.c` (`swapin_readahead`).
Upstream source: <https://github.com/gregkh/linux/tree/v6.18.48>.

Stable zero `memory.swap.current` means that cgroup owns no swap-slot return
obligation even with positive `memory.stat.swapcached`: remote reads of offlined-owner slots
can charge resident cache to the reader's RAM boundary. That cache gives other
owners no credit; the reader's spare native RAM still needs separate backing
before a batch. Inconsistent positive-slot accounting remains unknown and
inhibits acquisition.

Recovery acquisition requires the configured service's current MainPID,
`Restart=no` and `KillMode=control-group`. Leases retain its trusted systemd
InvocationID. Same-invocation descendants or missing manager observations retain
backing after helper death. A replacement can't pin the old lease: after
verified death of the recorded PID/start identity, a different verified
invocation proves the old control-group invocation stopped. Legacy leases
without invocation evidence still require an empty boundary for cleanup.

Discovery saves private, locked hints atomically in `/var/lib/amc-page-return`
(`--state` overrides it). Campaigns resume by host PID window, mapping offset
and virtual page address, even beyond 512 PIDs, 8,192 mappings and 8,388,608
pages, without walking empty cgroup descendants. Hints grant no authority:
identity, layout, placement, native backing and PTEs are rechecked. PID reuse,
ordinary exec or migration resets the target hint. Mapping offsets bind the
complete maps stream's SHA-256, hashed with a fixed 64 KiB buffer and 20-second
deadline. Large streams resume without total-byte truncation. Any changed VMA
sequence resets the process frontier even with unchanged exec/placement identity
and byte-offset boundary. Each bounded window requires a fresh matching maps
fingerprint; unstable streams stay incomplete. Full sweeps wrap to revisit
churn and same-layout exec. Waits/interruption retain the first unread range.

A scan-budget cutoff is not end-of-mm or successful recovery, even with zero
destination swap counters. Kernel counters must also show zero selected demand.
After restart, finishing an inherited suffix only advances discovery: success
requires a complete new PID sweep in the current campaign. Old hints cannot
certify residency of an interrupted prefix. Completion binds mappings,
present/swapped PTE state, lifetimes and placements across the selected frontier.
A page newly swapped behind a completed PID invalidates the sweep even if
charged outside the destination subtree.

```sh
amc recover-swap                 # bounded incremental return, devices stay on
amc recover-swap --whole-device  # explicit conservative swapoff/restore
amc recover-swap --restore       # restore declared devices after interruption
amc recover-swap --restore --restore-manifest /etc/amc-swap-restoration.json
```

Emergency units pass a root-owned, non-writable JSON array of declared recovery
targets via `--restore-manifest`. It restores missing devices without contacting
the broker, even after broker shutdown. The reader rejects symlinks,
oversized/empty or invalid manifests and writable/foreign-owned files. Require
`swapRestorationManifestVersion >= 1`. An active device at the wrong priority
is an error, not success; this restoration path does not perform an unbacked
swapoff to change its priority. Recovery helpers must match the configured
native RAM ceiling exactly, with zero swap.

Recovery can enter below the ordinary free-swap floor, but keeps the host RAM
reserve, pressure gates, helper backing and native ancestor checks. Bounded
batches can start without room for the full return debt. Other admission waits
while a campaign owns recovery. Cancellation, read failure and broker restart
keep the claim until native cleanup. Exit 75 means waiting, stalled or
incomplete return (including partial progress or unreadable pages). Page
recovery exits 0 only with zero nonresident return demand in selected subtrees.
The hosted receipt also proves the full fixture mapping is resident before the
target probes it and its bytes are intact. Retained resident swap slots are allowed and recorded. Whole-device recovery
requires backing for the full device and all affected native domains and
restores the device before reporting success. Consumers should install
`--restore` as maintenance-unit `ExecStopPost`; keep device restoration separate
from admission release.

The shared-admission VM covers drain/restart, warm nested launch,
namespace preservation, surviving descendants, real pageout/return, safe waits
and interrupted campaigns. These qualification gates don't prove live
Steam/Proton containment or successful recovery on a deployed workstation.
