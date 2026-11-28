# Persistent local admission

`amc admission serve` shares a durable byte budget across one Unix user's
independent processes. The service is optional; the Home Manager module can
install it. Canix owns fleet-specific contracts, user budget shares and native
slice limits. Systemd owns execution and cgroups. Ledgers are per machine and
user: cooperative memory admission is not a sandbox or fleet scheduler.

## Policy

The server reads a versioned JSON policy file with sizes in integer bytes. For
standalone slice/service setup, enrollment and lifecycle, see
[gaming-first workstation use](workstation-policy.md).

```json
{
  "version": 1,
  "budget_bytes": 134217728,
  "reserve_bytes": 1073741824,
  "queue_limit": 32,
  "contracts": {
    "example": {
      "slice": "agent-tools.slice",
      "memory_max": 67108864,
      "memory_swap_max": 0,
      "max_running": 1,
      "pause_file": null
    }
  }
}
```

These are **small fixture values**, not workstation defaults. Set finite native
`MemoryMax` and `MemorySwapMax` on the slice and keep the slice active through
server dependencies. Admission fails closed without observations of the slice,
controllers, host memory or ancestry. A pause marker blocks new admissions while
present; it does not revoke existing grants.

Each grant reserves the contract's full hard memory ceiling. Admission checks
the configured user budget and tightest live host/target-ancestor headroom, then
subtracts existing commitments. Host/ancestor capacity counts all commitments;
a slice counts its own and its descendants'. Admission subtracts the host safety
reserve only from host headroom. This deliberately double-counts resident
granted memory. Queues are FIFO per slice: a saturated long-lived server pool
cannot block a separate disposable-job pool. No automatic memory learning, CPU
scheduling, GPU-memory accounting or PSI threshold policy. Budget other users
and system workloads too.

## Commands

```sh
amc admission serve --policy /path/to/policy.json
amc admission status --json
amc admission exec --contract example --timeout 120 -- command argument
amc admission exec --contract example --timeout 120 --runtime-max-sec 300 -- command argument
```

`--timeout` bounds admission wait, not execution. The optional
`--runtime-max-sec` sets a systemd runtime bound for disposable jobs that
survives client/coordinator death; native stop time is extra. Long-lived servers
get no runtime bound by default unless one is explicitly requested.

For bounded work, opt into a patient per-user resource queue:

```sh
amc exec --contract example --queue --max-ram-usage 64MiB --runtime-max-sec 300 -- command argument
```

`--queue` requires both ceilings. Without `--timeout`, resource waits don't
expire implicitly unless the selected protected pool caps them. Legacy calls
default to 120 seconds. Grants get a fresh 30-second entry grace; native startup
has a separate limit of 30 seconds. Full queues return typed backpressure with
`retry_after_ms`, not execution failure for accepted work. Status and
cancellation still work at the queue bound.
Accepted tickets cannot replay commands: restart discards unentered work.

Each finite host reservation lasts one hour; queued helpers re-request it after
expiry. `Type=notify` defers native `RuntimeMaxSec` until `READY=1`, so host
queue time does not consume the runtime bound. See
[patient native execution](queued-execution.md) for readiness/completion,
qualification gates and consumer boundaries required before retiring lifetime
slots.

## Installation and removal

The service is optional; embeddable gates, `inspect`, `watch` and offline
reports need no service. Consumers own enrollment and native aggregate slices: install the CLI, declare
a finite slice, then supply the JSON policy above. To test manually, start
`admission serve` in a separate terminal and check `admission status` before
enrolling a finite command. Default paths require the user's runtime and state
directories.

Import the flake's `homeManagerModules.default`. Set
`services.amc.admission.enable = true`, `package` to the chosen pinned AMC
package, and `policy` to the consumer's JSON-shaped attribute set. The module
creates `amc-admission.service` with dependencies on its declared slices and
writes `amc/admission.json` under the XDG config directory. Consumers still
define and budget workload slices. The coordinator has its own native 128 MiB
limit and no swap, separate from workload limits.

For upgrades or policy changes, keep the ledger and restart the coordinator
through its native manager. Then check `amc admission status --json`: entered
work keeps its previous contract/weight until termination; unentered requests
are invalid. If restart fails, don't delete state, replay commands or run
unrestricted substitutes. Restore manager access or the last working
package/policy and let persisted identities reconcile. Keep malformed state for
diagnosis.

To disable enrollment, remove the consumer's admission wrapper/configuration
and explicitly choose how future work runs. Wait for entered work to finish or
verify and stop the exact owned native invocation. Before stopping the coordinator,
confirm zero commitments and no unreconciled entries in status. With Home Manager,
set `services.amc.admission.enable = false` and deploy that user's configuration
as usual. For a manual coordinator, stop only that server. Stopping it blocks new
managed starts; existing jobs keep their systemd limits.

To remove it, also uninstall the CLI/module through its original package manager
and remove unused consumer-owned bindings. Keep the private ledger until you
verify all recorded workloads have terminated. Only the consumer can explicitly
decide to remove native slices and limits. Never delete the ledger to free
capacity during install, restart, disable or uninstall.

The service's mechanism gates are the native admission fixture and explicit
two-user NixOS VM. The [Ubuntu record](ubuntu-diagnostics-20260930.md) verifies diagnostics only,
not this daemon or Home Manager installation/removal. Distribution-specific
setup and consumer rollout need separate acceptance evidence. A passing fixture
says nothing about gaming performance.

## State and execution

Status shows byte commitments, queued/reserved/running entries, typed wait
reasons, observed slice hard/swap ceilings for capacity-blocked requests and
unreconciled identities. Unknown native state never frees capacity.

The default socket is `$XDG_RUNTIME_DIR/amc/admission.sock`; durable state is
`$XDG_STATE_HOME/amc/admission` (falling back to `$HOME/.local/state`). Explicit
`--socket` and server `--state` paths must be absolute. State directories are
private, with an exclusive kernel file lock and atomic fsync/rename snapshots.
Startup refuses a malformed or missing initialized ledger rather than forgetting
reservations. The socket has a separate exclusive lock and accepts only same-UID
peers. Clients send contract names and server-generated tickets, never arbitrary
release instructions or foreign unit names.

Only the submitting peer can cancel; its PID and kernel start time must match,
so a copied public diagnostic ID cannot. Entry also needs a random one-use capability, issued only
in the enqueue reply, never in poll, status or the ledger. Expiry, cancellation
and coordinator restart invalidate it. The submitting PID/start time must still
be live. Helpers without a capability fail closed: upgrade clients and
coordinator together. A diagnostic ticket ID cannot authorize another
cooperative client, but same-UID process inspection and user-manager control
remain outside this isolation boundary.

`exec` preserves argv, cwd, streams and environment except native manager-owned
variables. It submits once through the existing systemd backend with hard/swap
limits, `Restart=no`, `OOMPolicy=kill` and control-group cleanup. A helper starts
inside the native service. **Before exec**, the coordinator verifies the helper's
peer PID, invocation, placement and kernel limits. The command runs only after
durable entry acknowledgment.

Host acquisition has one two-second frame-exchange deadline, capped by remaining
resource wait. Stalled frame I/O checks cancellation every 50 ms; a late grant
cannot authorize execution after cancellation or resource-wait expiry. The
native helper retains signal handling through private entry and checks
cancellation again before exec. Losing a grant reply does not release its
reservation until verified native cleanup. Socket connect still blocks, so
cancellation under a full listen backlog is unbounded. This deadline does not
establish untimed host waits or separate native startup from workload runtime;
both remain independent qualification requirements.

## Failure and recovery

- Queued and reserved tickets can expire or be cancelled. A late helper cannot
  execute without its ticket. Restart invalidates unentered tickets, including
  on policy change.
- Entered workloads keep their full reservation until verified termination,
  including surviving descendants. Client exit, timeout or socket loss cannot
  release it. Cancellation requests a stop of only the verified invocation.
- Restart restores entered identities and weights before admission. A verified
  new boot retires old-boot reservations. Current and stored boot IDs must be
  canonical kernel UUIDs; malformed IDs block startup. Within one boot, failed
  manager queries or replaced identities retain their commitments.
- Retirement requires an empty/collected recorded cgroup and manager confirmation
  of inactivity or unit collection. No submission is replayed.
- Persistence failure stops the server before acknowledgment. Native workloads
  keep their limits; recovery uses the last committed state.
- Unresolved native outcomes return a CLI error even if the launcher exited zero.
  This isn't completion evidence: inspect status before retrying.

The ledger stores contract/identity metadata, not arguments, environment values
or output. Normal user-manager visibility rules still apply to native systemd
unit metadata.

Admission manages native local tool services. A `nix build` client does not
contain daemon builders, containers or remote work; their lifecycle owners need
separate native boundaries. Long-lived application servers reserve their entire
domain for their lifetime and need a contract/pool sized accordingly.
