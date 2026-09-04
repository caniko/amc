# Generic proof results

Last local run: 2026-09-04T12:35:05Z on `atlas`.

- Result: **PASS**
- Unit: `app-amc-amc-proof-877416b4@46da86396861c6f8.service`
- First self-report cgroup: `/user.slice/user-1000.slice/user@1000.service/app.slice/app-amc.slice/app-amc-amc-proof-877416b4@46da86396861c6f8.service`
- `memory.max`: 268435456 bytes
- `memory.swap.max`: 33554432 bytes
- Maximum observed swap: 33349632 bytes
- `memory.events oom`: 1
- `memory.events oom_kill`: 2
- Unit result: `oom-kill`
- Heartbeat maximum gap: 50058 us
- Heartbeat p99 delay: 63 us
- Artifacts: `/data/nvme0/can/canix/projects/repos/owned/amc/proof-artifacts/20260904T123454Z`

The heartbeat survived and no `amc` process remained after cleanup. This proves only
the bounded local application-cgroup case; it does not attribute Nix daemon builders.

## VM acceptance

Last VM run: 2026-09-04 on `atlas`.

- Result: **PASS**
- Limits were verified before target execution.
- The Nix client ran in its AMC user cgroup.
- The synthetic derivation builder reported `0::/system.slice/nix-daemon.service`.
- A bounded OOM kill left the heartbeat responsive and cleanup removed the AMC unit.
