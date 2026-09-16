# Known Boundaries

Real applications retain their native lifecycle owner. AMC's private TOML and
`run`/`launch` commands are explicit fixture/compatibility tools, not normal-launch
policy or a public standard. Same-user manager privileges are not a sandbox.

Nix/Lix daemon builders do not inherit an invoking client's user-service policy.
The isolated `nix-broker-boundary` VM test retains that regression.
`programs.amc.nixBuildPool` remains blocked. Nothing in this corrective milestone
sets a whole-daemon cap, changes group killing, or restarts a broker.

The OpenCode backend and frontends, containers and remote work can be separate
execution domains. Diagnostics identify these known boundaries without inferring
general IPC causality. Configuring an activation client does not configure its
already-running server. Native OpenCode activation and UX remain unmeasured.

Default diagnostics omit application journal messages and arbitrary subprocess
error streams. Workload output forwarded by `amc run` is a distinct raw interface
and can contain sensitive text. Do not publish it as sanitized evidence.

Kernel observations are timestamped, sequential snapshots. Missing, malformed,
unreadable and disappeared measurements are unknown, not zero. Visible ancestry
ends at the current cgroup mount's root; hidden namespace ancestors are unknown.
Retained systemd state and open cgroup file descriptors do not guarantee retained
kernel counters. The finite VM observer must be ready before stress is released.

SIGINT/SIGTERM handling is bounded and scoped to one attempt. No cleanup is
promised after SIGKILL, uninterruptible kernel operations, or manager failure.
An absent/collected unit does not prove nothing ran. Unknown outcomes require
manual inspection of the original identity, never blind replay.

Helper checks verify settings at helper entry, before allocation. They do not
cover arbitrary loader or constructor execution. Stronger fail-closed
arbitrary-command launching remains unsupported rather than adding a supervisor.

The current A/B/C and high/max exercises are smoke tests only. Pressure response,
aggregate parent budgeting, launch/CPU/memory overhead and OpenCode usefulness
are deferred until executable mechanism gates pass. See the corrective ledger.
