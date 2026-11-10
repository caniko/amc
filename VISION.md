# AMC vision

AMC keeps a Linux workstation usable while memory-hungry work runs on it. It
does three related things: pass through cooperative memory admission for
applications that ask for a grant (`amc-runner`), observe systemd
services and scopes with passive telemetry (`amc-telemetry`, `amc inspect`,
`amc watch`, `amc report`), and optionally supervise enrolled native backends
with bounded restart or termination authority. Operators, agent harnesses and
application authors are the users: the Canix fleet consumes it to keep batch
analysis and editors alive next to each other.

## Problem

A workstation has one memory budget shared by a foreground session and any
number of compilers, browsers and batch jobs. Without coordination the kernel
chooses a victim at the worst moment - usually the editor, the terminal or the
agent. AMC makes that choice explicit: cooperative workloads reserve capacity
before they grow, observation stays passive and honest about what it did not
see, and any intervention is scoped to a named native unit rather than a
heuristic kill.

## Principles

- Observation is passive. `inspect` and `watch` never start, stop, move or
  reconfigure the target; `doctor` and the fixture tools are explicitly marked
  as not passive and run only deliberately.
- Unknown is not zero. Missing, malformed, unreadable or disappeared
  measurements carry a null value and a reason; completion is earned from
  verified lifetime continuity, never from matching paths alone.
- Real applications keep their native lifecycle owner. systemd retains
  execution, cgroup placement, hard limits and descendant cleanup; AMC's
  fixture and `run`/`launch` paths are compatibility tools, not a policy
  standard and not a sandbox.
- Reservations are conservative and non-revocable during a bounded task.
  Committed weight is added on top of observed usage, and pressure stops new
  admission instead of reclaiming memory in use.
- Authority is enumerated, not implied. Supervision policy version 1 offers
  `restart`, `terminate` and `observe`, and the read-only routes (`status`,
  `replay`) never signal or start anything.
- An outcome that was not observed is reported as UNKNOWN and requires manual
  inspection of the recorded identity; there is no blind replay.

## Scope

- `amc-runner`: embeddable admission for cooperative applications, including
  grants, persistent per-user admission, shared host broker and bounded queues.
- `amc-admission`: the client and broker contract, deadlines and entry
  acknowledgment.
- `amc-telemetry` and the CLI: bounded inspection, finite capture, offline
  reports and diffs for native services and scopes.
- `amc-supervision`: optional whole-host supervision of enrolled native services
  and admitted job pools.
- NixOS and Home Manager modules, standalone units, an example admission policy
  and a workstation guide that covers enrollment, deadlines, upgrade, disable
  and removal.

## Non-goals

- A general policy standard, sandbox, or transparent replacement for normal
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
landed or are landing, each with a qualification gate. Remaining work is
measurement and native evidence before any feature is presented as
production-ready.

## Place in the fleet

Canix consumes `amc` as a flake input: `root/packages/amc/default.nix`,
`flake.nix`, `home/profiles/compute/default.nix`,
`home/profiles/development/default.nix` and the NixOS supervision module.
Operators reach it through `docs/src/operations/amc-admission.md`; the `amc`
global skill in `ai-skills` carries the workstation-readiness checklist.
