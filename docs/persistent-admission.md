# Persistent local admission

`amc admission serve` shares a durable byte budget across independent processes
of one Unix user. The optional Home Manager module installs the service. Canix
owns fleet-specific contracts, user budget shares, and native slice limits.
Systemd owns execution and cgroups. Each machine and user has a separate ledger;
this is cooperative memory admission, not a sandbox or a fleet scheduler.

## Policy

The server accepts a versioned JSON file. All sizes are integer bytes:

For a complete standalone slice/service setup, enrollment and lifecycle
walkthrough, use [gaming-first workstation use](workstation-policy.md).

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

These are **small fixture values**, not workstation defaults. Declare the
slice with finite native `MemoryMax` and `MemorySwapMax`, and keep it active
through the server's dependencies. Admission fails closed if the slice,
controllers, host memory, or ancestry cannot be observed. A pause marker blocks
new admissions while present; it does not revoke existing grants.

Each grant reserves the contract's entire hard memory ceiling. Admission checks
the configured user budget and the tightest live host/target-ancestor headroom,
then subtracts existing commitments. Host/ancestor capacity accounts for all
commitments; a slice accounts for its own and its descendants' commitments.
The host safety reserve is subtracted from host headroom only. This deliberately
double-counts already resident granted memory. Queue ordering is FIFO per slice;
a saturated long-lived server pool cannot block a separate disposable-job pool. There is
no automatic memory learning, CPU scheduling, GPU-memory accounting, or PSI
threshold policy. Consumers must budget other users and system workloads too.

## Commands

```sh
amc admission serve --policy /path/to/policy.json
amc admission status --json
amc admission exec --contract example --timeout 120 -- command argument
amc admission exec --contract example --timeout 120 --runtime-max-sec 300 -- command argument
```

`--timeout` is the admission wait, not an execution deadline. The optional
`--runtime-max-sec` gives a disposable job a systemd-owned runtime bound that
survives client/coordinator death; native stop time is additional. It is omitted
for long-lived servers unless explicitly requested.

## Installation and removal

The service is optional. The embeddable gates, `inspect`, `watch` and offline
reports do not require it. Consumers own enrollment and the native aggregate
slices: install the CLI first, declare a finite slice, then supply the JSON
policy above. For a manually managed test session, start `admission serve` in a
separate terminal and use `admission status` before enrolling a finite command.
The server's default paths require the user's runtime and state directories.

The flake exports `homeManagerModules.default`. With that module imported, set
`services.amc.admission.enable = true`, `package` to the chosen pinned AMC
package, and `policy` to the consumer's JSON-shaped attribute set. The module
creates `amc-admission.service` with dependencies on its declared slices and
writes `amc/admission.json` under the XDG config directory. It does not define
the workload slices or choose their budgets. The coordinator has its own 128 MiB
native memory limit and no swap, separate from workload limits.

For an upgrade or policy change, preserve the ledger and restart the coordinator
through its native manager. Check `amc admission status --json` afterward:
entered work retains its previous contract/weight until termination; unentered
requests are invalidated. An unsuccessful restart is not permission to delete
state, replay commands, or run unrestricted substitutes. Restore manager access
or the last known-working package/policy and let the persisted identities
reconcile. Keep malformed state for diagnosis.

To disable enrollment, remove the consumer's admission wrapper/configuration
and explicitly decide how future work should run. Wait for entered work to
finish, or stop an exact owned native invocation after verifying its identity.
Confirm zero commitments and no unreconciled entries in status before stopping
the coordinator. For Home Manager, set `services.amc.admission.enable = false`
and apply that user's configuration through the consumer's normal deployment
workflow. For a manual coordinator, terminate only that server. Stopping it
refuses new managed starts; existing jobs retain their systemd limits.

Removal additionally uninstalls the CLI/module through its original package
manager and removes consumer-owned bindings that are no longer needed. Preserve
the private ledger until all recorded workloads are verified terminated. Native
slices and their limits belong to the consumer and require that owner's explicit
removal decision. No install, restart, disable or uninstall action should delete
the ledger as a way to release capacity.

The native admission fixture and explicit two-user NixOS VM are the service's
mechanism gates. The [Ubuntu record](ubuntu-diagnostics-20260930.md) verifies
diagnostics, not this daemon or its Home Manager installation/removal path.
Distribution-specific service setup and consumer rollout need their own
acceptance evidence; passing the fixture is not a gaming-performance claim.

## State and execution

Status includes byte commitments, queued/reserved/running entries, typed wait
reasons, observed slice hard/swap ceilings for capacity-blocked requests, and
unreconciled identities. An unknown native state never frees capacity.

The default socket is `$XDG_RUNTIME_DIR/amc/admission.sock`; durable state is
`$XDG_STATE_HOME/amc/admission` (falling back to `$HOME/.local/state`). Explicit
`--socket` and server `--state` paths must be absolute. State directories are
private, with an exclusive kernel file lock and atomic fsync/rename snapshots.
A malformed or missing initialized ledger blocks startup instead of forgetting
reservations. The socket also has its own exclusive lock. Only same-UID peers
may use it. Clients carry contract names and server-generated tickets, never
arbitrary release instructions or foreign unit names.
Cancellation is bound to the submitting peer PID and its kernel start time;
another process cannot cancel a ticket by copying its public diagnostic ID.
Entry additionally requires a random one-use capability issued only in the
enqueue reply. Poll, status and the durable ledger omit it; expiration,
cancellation and coordinator restart invalidate it. The submitting PID/start
time must still be live. Native helpers without a capability fail closed, so
upgrade clients and coordinator together. This prevents a diagnostic ticket ID
from authorizing another cooperative client; same-UID process inspection and
user-manager control remain outside this interface's isolation boundary.

`exec` preserves argv, cwd, streams, and the caller's environment except native
manager-owned variables. It submits once through the existing systemd backend,
with hard/swap limits, `Restart=no`, `OOMPolicy=kill`, and control-group cleanup.
An internal helper starts inside the native service and asks the coordinator to
verify its peer PID, invocation, placement, and kernel limits **before exec**.
Only a durably recorded entry acknowledgment permits the actual command to run.

## Failure and recovery

- Queued and reserved tickets may expire or be cancelled. A late helper cannot
  execute after its ticket disappears. These unentered tickets are invalidated
  on coordinator restart, including a policy change.
- Entered workloads retain their full reservation until verified termination,
  including surviving descendants. Client exit, timeout, or socket loss cannot
  release them. Cancellation requests a stop of only the verified invocation.
- After restart, entered identities and weights are restored before admission.
  A verified new machine boot retires old-boot reservations. Both current and
  stored boot IDs must be canonical kernel UUIDs; malformed IDs block startup.
  Within one boot, a failed manager query or a replaced identity stays accounted for.
- A workload is retired only after its recorded cgroup is empty/collected and
  the manager confirms inactivity or unit collection. No submission is replayed.
- Persistence failure stops the server before it acknowledges the change. Its
  native workloads retain their limits. Recovery uses the last committed state.
- An unresolved native outcome returns a CLI error even if the launcher exited
  zero. It is not completion evidence; inspect status before retrying.

The ledger stores contract/identity metadata, not command arguments, environment
values, or output. Native systemd unit metadata remains subject to the normal
user-manager visibility rules.

This interface manages native local tool services. A `nix build` client does not
contain daemon builders, containers, or remote work; their lifecycle owners need
their own native boundaries. Long-lived application servers reserve their whole
domain for their lifetime and need an appropriately sized contract/pool.
