# AMC vision

AMC keeps Linux workstations usable during memory-hungry work. It passes through
cooperative memory admission requests for applications (`amc-runner`), passively
observes systemd services and scopes (`amc-telemetry`, `amc inspect`, `amc watch`,
`amc report`), and optionally supervises enrolled native backends with bounded
restart or termination authority. It's for operators, agent harnesses and
application authors. The Canix fleet uses it to keep batch analysis and editors
running side by side.

## Problem

The foreground session shares one memory budget with compilers, browsers and
batch jobs. Without coordination, the kernel picks a victim at the worst moment,
usually the editor, terminal or agent. AMC makes that choice explicit: cooperative
workloads reserve capacity before growing, passive observation reports gaps,
and interventions target named native units rather than killing by heuristic.

## Principles

- Observation is passive: `inspect` and `watch` never start, stop, move or
  reconfigure the target. `doctor` and fixture tools are marked non-passive;
  run them only deliberately.
- Unknown is not zero. Missing, malformed, unreadable or disappeared
  measurements carry null and a reason; completion requires verified lifetime
  continuity, and matching paths alone never suffice.
- Real applications keep their native lifecycle owner. Systemd owns
  execution, cgroup placement, hard limits and descendant cleanup; AMC's
  fixture and `run`/`launch` paths are compatibility tools, not a policy
  standard or sandbox.
- Reservations are conservative and non-revocable during a bounded task.
  Accounting adds committed weight to observed usage. Pressure stops new
  admission instead of reclaiming memory in use.
- Authority is enumerated, not implied. Supervision policy version 1 offers
  `restart`, `terminate` and `observe`, and the read-only routes (`status`,
  `replay`) never signal or start anything.
- AMC reports unobserved outcomes as UNKNOWN. Inspect the recorded identity
  manually; AMC doesn't replay blindly.

## Scope

- `amc-runner`: embeddable admission for cooperative applications, including
  grants, persistent per-user admission, shared host broker and bounded queues.
- `amc-admission`: client and broker contract, deadlines and entry
  acknowledgment.
- `amc-telemetry` and the CLI: bounded inspection, finite capture, offline
  reports and diffs for native services and scopes.
- `amc-supervision`: optional whole-host supervision of enrolled native services
  and admitted job pools.
- NixOS and Home Manager modules, standalone units, an example admission policy
  and a workstation guide that covers enrollment, deadlines, upgrade, disable
  and removal.

## Non-goals

- A general policy standard, sandbox or transparent replacement for normal
  launching.
- Reclaiming memory already in use, whole-daemon caps for the Nix builder, or
  restarting a broker to recover capacity (`nixBuildPool` remains blocked).
- Inferring general IPC causality across separate execution domains such as
  containers, remote work or unrelated frontends.
- Promising cleanup after SIGKILL, uninterruptible kernel operations or manager
  failure.
- Aggregate parent budgeting, launch/CPU/memory overhead conclusions and
  OpenCode usefulness claims before executable mechanism gates pass; the current
  A/B/C and high/max exercises are smoke tests only.

## Direction

The corrective ledger is the active roadmap: native request-age clocks, bounded
patient queues, protected small-call pools, advance foreground preparation,
finite completion lanes, swap-return recovery and supervision replay binding
have landed or are underway, each with a qualification gate. Remaining work is
measurement and native evidence before any feature is presented as
production-ready.

## Place in the fleet

Canix consumes `amc` as a flake input: `root/packages/amc/default.nix`,
`flake.nix`, `home/profiles/compute/default.nix`,
`home/profiles/development/default.nix` and the NixOS supervision module.
Operators reach it through `docs/src/operations/amc-admission.md`; the `amc`
global skill in `ai-skills` carries the workstation-readiness checklist.
