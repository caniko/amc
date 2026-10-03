# Memory admission contract

Behavioral contract between cooperating applications and the workstation.
Normal invocation is unchanged (`bekiper run …`); registration and
negotiation happen inside the application through `amc-runner`.

## Grant

A grant authorizes one workload to start within a declared memory ceiling
(`weight` bytes). The coordinator reserves that capacity before launch; the
application schedules within it; native controls bound overshoot. A grant is
permission to grow up to the ceiling, not a promise that exactly `weight`
bytes are free right now.

## Lifecycle

- **Request**: application declares `app`, `weight > 0`, and a wait deadline.
- **Hold**: the reservation is held until the workload's confirmed
  termination, including detached workers that outlive submission.
- **Release**: only on confirmed termination or coordinator-observed
  collection of the workload domain. Never on timeout, lost connection, or
  process exit of the submitting client alone.
- **Grants are non-revocable** during a bounded task. Under pressure the
  coordinator stops new admissions and asks applications to lower future
  concurrency; it does not reclaim memory already in use.

## Accounting

- Reservations are conservative: committed weight is added on top of live
  observed usage. Overlap with already-charged RSS is accepted as a safety
  margin until measured worker RSS exists.
- Growth beyond the granted ceiling requires a new admission decision.
  Overshoot fails inside the workload's native boundary; it never silently
  extends the grant.

## Headroom and non-participants

The coordinator cannot control browsers, compilers, or other
non-participants. Safety comes from three layers together:

1. A real headroom reserve (bytes, not a fraction of hope).
2. An aggregate background boundary (`MemoryHigh`/`MemoryMax`/`MemorySwapMax`)
   that caps all analysis work together.
3. Applications that stop admitting and wind down to safe points when
   headroom or pressure violates policy.

## Scope and trust (initial)

- Per-user coordinator. It authenticates local clients and verifies which
  resource domains they own. No client may release another application's
  grant or nominate foreign units for cleanup.
- The `Coordinator` type is in-process (see `coordinator.rs`). The optional
  [persistent admission service](persistent-admission.md) adds a private Unix
  socket, durable workload identities, and an authenticated native entry gate.

## Unavailable coordinator

When managed operation is configured and the coordinator is unreachable,
applications refuse to start new heavy work and report the problem. They
never silently fall back to unrestricted execution. Existing workloads stay
under their native limits. A lease timeout marks a client unresponsive but
does not free its reservations; restart reconciles native workload
identities before granting new capacity.
