# Atlas host capability report

Observed 2026-09-04 before implementation. Commands were run locally on Atlas.
Only selected Nix settings were captured; credential-bearing configuration was
not recorded.

## Platform

```text
$ uname -a
Linux atlas 7.2.0-cachyos-lto #1-NixOS SMP PREEMPT_DYNAMIC Tue Jan  1 00:00:00 UTC 1980 x86_64 GNU/Linux

$ systemctl --version
systemd 261 (261.1)

$ nix --version
nix (Nix) 2.34.8

$ nix config show | rg '^(use-cgroups|experimental-features|max-jobs|cores) ='
cores = 4
experimental-features = configurable-impure-env fetch-tree flakes nix-command pipe-operators
max-jobs = 2
use-cgroups = false
```

## Cgroups and user manager

```text
$ findmnt -t cgroup2 -o TARGET,SOURCE,FSTYPE,OPTIONS
/sys/fs/cgroup cgroup2 cgroup2 rw,nosuid,nodev,noexec,relatime,nsdelegate,memory_recursiveprot,memory_hugetlb_accounting

$ cat /sys/fs/cgroup/cgroup.controllers
cpuset cpu io memory hugetlb pids rdma misc dmem

$ systemctl --user is-system-running
degraded
```

The user manager was reachable despite the degraded state; one unrelated
`skillnet-subscription-sync.service` was failed. `systemctl --user
show-environment` succeeded.

The harmless transient probe accepted `Slice=app.slice`,
`MemoryAccounting=yes`, `MemoryMax=134217728`, `MemorySwapMax=0`,
`OOMPolicy=kill`, and `KillMode=control-group`. `systemctl --user show` reported
the same values. Its cgroup files contained:

```text
memory.max       134217728
memory.swap.max  0
memory.oom.group 1
```

`MemoryOOMGroup=yes` itself was rejected as `Unknown assignment` and is absent
from the systemd 261 manuals. The documented `OOMPolicy=kill` behavior produced
the required kernel value, so AMC verifies the kernel file instead of sending a
nonexistent property.

## Memory and swap

At inspection time the host had 64,907,411,456 bytes RAM and 41,184,194,560
bytes swap. Swap consisted of a 17,179,865,088-byte reserve file and a
24,004,329,472-byte partition; about 10.2 GB was in use. `/proc/meminfo`
reported zswap activity. The running OpenCode service used approximately 1.34
GB memory and 1.18 GB swap at that instant with no cgroup limit, motivating the
prepared 6 GiB memory / 512 MiB swap trial profile but not proving that profile
safe for every workload.

Global memory PSI at inspection was `avg10=0.00` for both `some` and `full`.

## Nix daemon boundary

`nix-daemon.socket` was enabled and `nix-daemon.service` active. Atlas places it
at `/canix.slice/canix-background.slice/nix-daemon.service`, with a host-level
`MemoryMax=55834574848`, unlimited swap, and `OOMPolicy=continue`. Multi-user
builders therefore do not inherit an AMC user-service contract.
