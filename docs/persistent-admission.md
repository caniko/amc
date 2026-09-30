# Persistent local admission

`amc admission serve` shares a durable byte budget across independent processes
of one Unix user. The optional Home Manager module installs the service. Canix
owns fleet-specific contracts, user budget shares, and native slice limits.
Systemd owns execution and cgroups. Each machine and user has a separate ledger;
this is cooperative memory admission, not a sandbox or a fleet scheduler.

## Policy

The server accepts a versioned JSON file. All sizes are integer bytes:

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
```

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
  A verified new machine boot retires old-boot reservations. Within one boot,
  a failed manager query or a replaced identity stays accounted for.
- A workload is retired only after its recorded cgroup is empty/collected and
  the manager confirms inactivity or unit collection. No submission is replayed.
- Persistence failure stops the server before it acknowledges the change. Its
  native workloads retain their limits. Recovery uses the last committed state.

The ledger stores contract/identity metadata, not command arguments, environment
values, or output. Native systemd unit metadata remains subject to the normal
user-manager visibility rules.

This interface manages native local tool services. A `nix build` client does not
contain daemon builders, containers, or remote work; their lifecycle owners need
their own native boundaries. Long-lived application servers reserve their whole
domain for their lifetime and need an appropriately sized contract/pool.
