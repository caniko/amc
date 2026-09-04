# v0.1 design

AMC v0.1 is a command-line proof harness with no resident process. It submits a
transient user `.service` through `systemd-run --user`, and systemd creates the
final cgroup and applies hard memory controls before `execve` starts the target.

```text
caller -> amc -> systemd-run -> user systemd -> transient .service -> target
                                      |                    |
                                      +-- cgroup v2 -------+
```

The contract is deliberately only `MemoryMax`, `MemorySwapMax`,
`OOMPolicy=kill`, `KillMode=control-group`, and memory accounting. There is no
`MemoryHigh`, soft protection, oomd policy, restart, CPU/I/O control, dynamic
policy, or monitor.

`systemd-run` is used instead of a Rust D-Bus client because it already handles
PTY and pipe attachment, environment transfer, transient units, and startup
errors. Replace it only if measured launch overhead, missing diagnostics, or a
required integration cannot be met through the CLI.

Systemd 261 does not expose the proposed `MemoryOOMGroup=` property.
`OOMPolicy=kill` sets the kernel's `memory.oom.group` to `1`; `amc doctor` probes
that cgroup file directly. Supporting a systemd version where this implication
does not hold is a hard failure, not a degraded mode.
