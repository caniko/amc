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

Shared namespace execution additionally requires `namespace_runner_bytes`
(64 MiB–1 GiB per enrolled non-root UID). All of a user's host runners and
preparation helpers share
`app-amchostrunner.slice` at that exact aggregate RAM ceiling and zero swap;
individual services retain their 64 MiB cap. The host module creates the slice.
Policy validation requires the sum of these aggregate envelopes to fit inside
`reserve_bytes`. The ledger retains previously enrolled UIDs and their ceilings
across restart, user removal and allowance reductions until the aggregate is
positively empty. Startup also discovers surviving aggregate slices from older
snapshots. If retained plus current allowances exceed a reduced host reserve,
new admission waits while existing native work completes.

Native projections back them in finite ancestors, including
while runners wait, and the runner authenticates this allowance with the broker
before submitting the inner shared job. They are static maintenance backing,
so preparations do not wait for a runner that is itself waiting for admission.
Host status reports `namespace_runner_reserved_bytes`; consumers can require
`lib.admissionNamespaceRunnerVersion = 2` for namespace runners and concurrent
preparation helpers (version 1 covered only namespace runners). Without an allowance, shared
namespace execution fails before payload submission.

Policies with preparation profiles must declare a backed runner allowance;
zero remains valid when preparations are disabled. Profile names are forwarded
as a single literal option value, including names that begin with hyphens.

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

An optional `envelopes` map reduces `memory_bytes`/`swap_bytes` for named child
domains. Each override must select an allowed domain, fit that native domain,
and stay within the default child ceiling. Escrow sums the individual lanes;
their limits are persisted with the parent and survive policy reloads. Consumers
require `admissionContinuationEnvelopesVersion >= 1` before emitting overrides.
Use `amc admission host-policy-check --policy FILE` to validate the actual emitted
policy without starting a broker.
Root completion domains use fixed native pool requests: their effective envelope
must equal the domain's RAM and swap ceilings. Smaller overrides only apply to
individually enforced non-root children.

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

Linux keeps the original swap-entry memcg charge when a process migrates. If
that memcg is offline, the direct fault path can use the target mm, but
`__read_swap_cache_async` passes a null mm and falls back to the current reader's
memcg. For a remote `/proc/PID/mem` read, that is the finite recovery helper.
A frozen cgroup can still be migrated. A swapped PTE does not expose its charge
owner to this userspace API, and a single mm can contain mixed-origin pages.
Consequently each batch conservatively checks every online cgroup with stable
nonresident swap demand, plus both the current target and reader fallbacks, against their native
ancestors and commitments. This can wait for an unrelated charged domain to
gain headroom; it cannot borrow the destination grant to cover another owner.
The native VM requires separate proofs of online original charging, frozen
migration, offlining fallback charge attribution and all headroom denials with zero OOM events. See Linux
[`mem_cgroup_swapin_charge_folio`](https://github.com/gregkh/linux/blob/v6.18.48/mm/memcontrol.c#L4779-L4799)
and [cgroup memory ownership](https://docs.kernel.org/admin-guide/cgroup-v2.html#memory-ownership).
The [swap-cache call site](https://github.com/gregkh/linux/blob/v6.18.48/mm/swap_state.c#L480)
is essential: a remote target's placement alone does not identify the fallback
owner. Returned pages must fit the helper's remaining native resident allowance
as well as its bounded working memory; successive batches wait when it is full.
Charge-owner inventory is bounded to 65,536 groups, depth 256 and 20 seconds;
an incomplete inventory rejects acquisition without faulting pages. Root recovery
acquisition RPCs allow 30 seconds so a complete wide scan is not discarded at
the ordinary two-second program-call deadline. Device inventory also has a
20-second deadline, within the same maintenance RPC budget.

Both page and device inventories run in one bounded background task outside
the broker accept loop. Ordinary status, preparation, and acquisition RPCs
remain responsive. No scan clone can write the ledger or grant a lease: the
broker rejects stale backing, replaced helpers, disconnected peers and results
older than one second, then repeats native identity, host capacity and local
fallback checks before persisting the same lease. Changed ownership or demand
cannot be transferred from an abandoned scan.
Inventory backing includes granted native obligations and Ready/continuation
escrow, while ungranted queue churn cannot discard a valid scan.
Whole-device replay also verifies the complete scanned cgroup frontier and
recalculates headroom from live limits, resident bytes and uncovered swap demand,
including groups with zero demand in the earlier scan. This final replay has a
250 ms bound and rejects incomplete or unsafe observations before durable grant.
Burst manager evidence is refreshed in a separate single background task and
expires after one second; missing evidence delays admission while kernel limits
remain directly rechecked. Manager latency cannot hold ordinary broker RPCs.

### Kernel page-return guard (version 1)

Incremental page return requires the `pageReturnKernelGuardVersion = 1` package
capability and a booted kernel carrying both patches in `nix/kernel/`. The
host-admission module adds them whenever page-return subtrees are configured;
standalone consumers can import `nixosModules.page-return-guard`. The patch
baseline is Linux 6.18.48. A package capability alone does not prove the running
kernel supports the interface: missing or unproven native guards return status
75 before any recovery read.

Root opens `/proc/<pid>/amc_mem`, a read-only, ptrace-authorized proc-mem
descriptor additionally requiring `CAP_SYS_ADMIN` in the initial user namespace.
Its open serializes with cgroup migration and pins the **same mm served by its
reads** and that target's memory cgroup. Kernel fdinfo reports guard version,
opaque mm cookie and memcg inode. Both the helper and target descriptors remain
open through inventory, grant, remote reads and broker-verified settlement. The
broker independently opens the current guarded mms and authenticates matching
kernel cookies in the native helper's descriptor table; there is no request-body
guard assertion. Both cookies persist in the recovery lease and must match again
at settlement, so exec or last-close/reopen cannot recycle an old grant into a
replacement mm. Descriptor inventories are bounded in count, size and time.

While guarded, memory-controller migration returns `EBUSY` for any task sharing
the mm. `CLONE_VM | CLONE_INTO_CGROUP` cannot create a migration bypass. The
pinned cgroup and ancestry cannot be removed or have their controller/type
configuration changed. The fallback memcg stays pinned even if the mm owner
dies and an existing cross-cgroup mm sharer survives. A new private fork/exec mm
starts unguarded; an obsolete proc descriptor cannot reach its replacement.
The last guarded FD close clears the pin with RCU-safe CSS lifetime management;
duplication, helper death and broker restart retain normal kernel FD semantics.

Guarded swap-in uses order-0 faults without swap readahead, so a backed batch
cannot allocate an unreserved larger folio or neighboring cache pages. Kernel
fdinfo exposes successful direct/cache swap-in page counts. The disposable VM
forces migration **after grant and before read** on regular-swap cache and zram
direct paths, including offlined original owners, reader migration, duplicated
FDs, broker restart and helper-loss cleanup. Receipts require exact per-page
`kpagecgroup` attribution, complete batch residency and zero OOM counters. A
separate native lifetime fixture checks pre-existing cross-cgroup mm sharers,
conflicting guard acquisition, `clone3` placement, private-fork initialization,
owner death, memcg offlining/controller changes, exec and last-FD cleanup.

The guard module selects the bounded-fault patch for Linux 6.18 or 7.2 and
rejects unsupported series at evaluation. Hosted qualification runs the full
shared-admission VM on both the pinned Nixpkgs kernel and Canix's exact pinned
CachyOS 7.2.8 source with ThinLTO and baseline CPU settings. The latter covers
the changed 7.2 swap-cache allocator and records the running kernel series.
Atlas's Zen4-optimized build and runtime remain consumer qualification gates.

Source anchors for this extension are Linux 6.18.48 `fs/proc/base.c` (`mem_open`,
`mem_rw`, `mem_release`), `kernel/cgroup/cgroup.c` (`cgroup_migrate_execute`,
`cgroup_can_fork`, controller/type writes and `cgroup_destroy_locked`),
`mm/memcontrol.c` (`get_mem_cgroup_from_mm`, `mem_cgroup_swapin_charge_folio`),
`mm/memory.c` (`alloc_swap_folio`) and `mm/swap_state.c` (`swapin_readahead`).
Upstream source: <https://github.com/gregkh/linux/tree/v6.18.48>.

A stable zero `memory.swap.current` means a cgroup owns no swap-slot return
obligation even when `memory.stat.swapcached` is positive: remote reads of
offlined-owner slots can charge resident cache to the reader's RAM boundary.
That cache supplies no credit to another owner, and the reader's unused native
RAM is still separately backed before a batch. Positive-slot inconsistent
accounting remains unknown and inhibits acquisition.

Recovery acquisition requires the configured service's current MainPID,
`Restart=no`, and `KillMode=control-group`. Leases retain its trusted systemd
InvocationID. Same-invocation descendants and missing manager observations keep
backing after the helper dies. A replacement cannot pin the old lease: once the
recorded PID/start identity is positively dead, a different verified invocation
proves the previous control-group invocation was stopped. Legacy leases without
invocation evidence retain empty-boundary-only cleanup.

Discovery keeps private, locked, atomically saved hints in
`/var/lib/amc-page-return` (`--state` overrides the directory). Campaigns resume
through host PID windows, streaming mapping offsets and virtual page addresses,
including beyond 512 PIDs, 8,192 mappings and 8,388,608 pages. This avoids walking
empty cgroup descendants. Identity, layout, placement, native backing and PTEs
are rechecked; hints grant no authority. PID reuse, ordinary exec and migration
reset the target hint; full sweeps wrap to revisit mapping churn and same-layout
exec. Waits/interruption retain the first unread range. A scan-budget cutoff is
distinct from end-of-mm and cannot establish successful recovery, even with zero
destination swap counters. Kernel counters must also show zero selected demand.
On restart, completing an inherited suffix only advances discovery: success
requires a complete new PID sweep within the current campaign. Old cursor hints
cannot certify that an interrupted prefix remains resident.

```sh
amc recover-swap                 # bounded incremental return, devices stay on
amc recover-swap --whole-device  # explicit conservative swapoff/restore
amc recover-swap --restore       # restore declared devices after interruption
amc recover-swap --restore --restore-manifest /etc/amc-swap-restoration.json
```

Emergency units pass a root-owned, non-writable JSON array of declared recovery
targets through `--restore-manifest`. This path restores missing devices without
contacting the broker, including after broker shutdown. The reader rejects
symlinks, oversized/empty or invalid manifests and writable/foreign-owned files.
Consumers require `swapRestorationManifestVersion >= 1`. An already-active device
at the wrong priority is an error, never a restoration success; this path does
not perform an unbacked swapoff to change that priority. Recovery helpers must
match the configured native RAM ceiling exactly, with zero swap.

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
