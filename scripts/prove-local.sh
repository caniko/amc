#!/usr/bin/env bash
set -euo pipefail

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
amc=${AMC_BIN:-"$root/target/debug/amc"}
config=${AMC_CONFIG:-"$root/examples/config.toml"}
artifacts=${1:-"$root/proof-artifacts/$(date -u +%Y%m%dT%H%M%SZ)"}
unit=
heartbeat_pid=
watcher_pid=

cleanup() {
  if [[ -n ${heartbeat_pid:-} ]] && kill -0 "$heartbeat_pid" 2>/dev/null; then
    kill "$heartbeat_pid" 2>/dev/null || true
    wait "$heartbeat_pid" 2>/dev/null || true
  fi
  if [[ -n ${watcher_pid:-} ]] && kill -0 "$watcher_pid" 2>/dev/null; then
    kill "$watcher_pid" 2>/dev/null || true
    wait "$watcher_pid" 2>/dev/null || true
  fi
  if [[ -n ${unit:-} ]]; then
    systemctl --user stop "$unit" >/dev/null 2>&1 || true
    systemctl --user reset-failed "$unit" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT INT TERM

mkdir -p "$artifacts"
if [[ ! -x $amc ]]; then
  cargo build --locked
fi

"$amc" doctor --json >"$artifacts/doctor.json"
systemctl --user --failed --no-legend >"$artifacts/failed-units-before.txt" || true
cat /proc/pressure/memory >"$artifacts/global-memory-pressure-before.txt"

"$amc" test heartbeat \
  --samples "$artifacts/heartbeat-samples.json" \
  --summary "$artifacts/heartbeat-summary.json" \
  --interval-ms 50 --duration-ms 10000 \
  >"$artifacts/heartbeat.log" 2>&1 &
heartbeat_pid=$!

"$amc" --config "$config" launch --id amc.proof --retain-unit -- \
  "$amc" test hog \
  --report "$artifacts/offender-self-report.json" \
  --expect-memory-max 256MiB \
  --expect-memory-swap-max 32MiB \
  --expect-memory-oom-group 1 \
  --chunk-size 8MiB --maximum 1GiB --progress-interval 64MiB --delay-ms 50 \
  >"$artifacts/launch.log" 2>&1

unit=$(sed -n 's/^AMC unit: //p' "$artifacts/launch.log" | sed -n '1p')
if [[ -z $unit ]]; then
  printf 'could not determine AMC unit; see %s\n' "$artifacts/launch.log" >&2
  exit 1
fi
printf '%s\n' "$unit" >"$artifacts/unit.txt"

for _ in $(seq 1 200); do
  [[ -s $artifacts/offender-self-report.json ]] && break
  sleep 0.05
done
[[ -s $artifacts/offender-self-report.json ]]

systemctl --user show "$unit" \
  -p Id -p ActiveState -p SubState -p Result -p ExecMainCode -p ExecMainStatus \
  -p Slice -p ControlGroup -p MemoryMax -p MemorySwapMax -p OOMPolicy -p KillMode \
  >"$artifacts/unit-start.properties"
cgroup=$(sed -n 's/^ControlGroup=//p' "$artifacts/unit-start.properties")
[[ $cgroup == /* ]]
cgroup_dir=/sys/fs/cgroup$cgroup

cat >"$artifacts/watch-cgroup.py" <<'PY'
import os
import pathlib
import sys
import time

cgroup = pathlib.Path(sys.argv[1])
output = pathlib.Path(sys.argv[2])
names = [
    "cgroup.events", "memory.current", "memory.events", "memory.max",
    "memory.oom.group", "memory.peak", "memory.pressure",
    "memory.swap.current", "memory.swap.max",
]
handles = {name: (cgroup / name).open() for name in names if (cgroup / name).is_file()}
maximum_swap = 0
while True:
    values = {}
    removed = False
    for name, handle in handles.items():
        try:
            handle.seek(0)
            values[name] = handle.read()
        except OSError:
            removed = True
            break
        (output / f"{name}.last").write_text(values[name])
    maximum_swap = max(maximum_swap, int(values.get("memory.swap.current", "0").strip()))
    if removed or "populated 0" in values.get("cgroup.events", ""):
        break
    time.sleep(0.005)
(output / "max-swap-bytes.txt").write_text(f"{maximum_swap}\n")
for handle in handles.values():
    handle.close()
fd = os.open(output / "max-swap-bytes.txt", os.O_RDONLY)
os.fsync(fd)
os.close(fd)
PY
python3 "$artifacts/watch-cgroup.py" "$cgroup_dir" "$artifacts" &
watcher_pid=$!

deadline=$((SECONDS + 20))
while ((SECONDS < deadline)); do
  state=$(systemctl --user show "$unit" -p ActiveState --value)
  [[ $state == failed || $state == inactive ]] && break
  sleep 0.05
done
if [[ $state != failed && $state != inactive ]]; then
  printf 'offender did not terminate within 20 seconds\n' >&2
  exit 1
fi
wait "$watcher_pid"
watcher_pid=
max_swap=$(<"$artifacts/max-swap-bytes.txt")

wait "$heartbeat_pid"
heartbeat_pid=
systemctl --user show "$unit" \
  -p Id -p ActiveState -p SubState -p Result -p ExecMainCode -p ExecMainStatus \
  -p Slice -p ControlGroup -p MemoryMax -p MemorySwapMax -p OOMPolicy -p KillMode \
  >"$artifacts/unit-final.properties"
journalctl --user-unit "$unit" -n 100 --no-pager >"$artifacts/unit.journal" || true
cat /proc/pressure/memory >"$artifacts/global-memory-pressure-after.txt"
systemctl --user --failed --no-legend >"$artifacts/failed-units-after.txt" || true

report_cgroup=$(jq -r .cgroupPath "$artifacts/offender-self-report.json")
report_memory=$(jq -r .memoryMax "$artifacts/offender-self-report.json")
report_swap=$(jq -r .memorySwapMax "$artifacts/offender-self-report.json")
report_oom_group=$(jq -r .memoryOomGroup "$artifacts/offender-self-report.json")
oom=$(awk '$1 == "oom" { print $2 }' "$artifacts/memory.events.last")
oom_kill=$(awk '$1 == "oom_kill" { print $2 }' "$artifacts/memory.events.last")
max_gap=$(jq -r .maxGapUs "$artifacts/heartbeat-summary.json")
p99=$(jq -r .p99DelayUs "$artifacts/heartbeat-summary.json")
result=$(sed -n 's/^Result=//p' "$artifacts/unit-final.properties")

[[ $report_cgroup == *"/$unit" ]]
[[ $report_memory == 268435456 ]]
[[ $report_swap == 33554432 ]]
[[ $report_oom_group == 1 ]]
((oom > 0))
((oom_kill > 0))
((max_swap <= 33558528)) # One page allows for a sampling/accounting race.
((max_gap < 2000000))
((p99 < 500000))
[[ $result == oom-kill || $result == signal ]]

systemctl --user stop "$unit" >/dev/null 2>&1 || true
systemctl --user reset-failed "$unit" >/dev/null 2>&1 || true
unit=
sleep 0.1
if pgrep -x amc >/dev/null; then
  printf 'an amc process remains after proof cleanup\n' >&2
  exit 1
fi

jq -n \
  --arg status pass --arg cgroup "$report_cgroup" --arg unit "$(<"$artifacts/unit.txt")" \
  --arg result "$result" --argjson memoryMax "$report_memory" \
  --argjson memorySwapMax "$report_swap" --argjson maxObservedSwap "$max_swap" \
  --argjson oom "$oom" --argjson oomKill "$oom_kill" \
  --argjson maxGapUs "$max_gap" --argjson p99DelayUs "$p99" \
  '{status: $status, unit: $unit, cgroup: $cgroup, result: $result,
    memoryMax: $memoryMax, memorySwapMax: $memorySwapMax,
    maxObservedSwap: $maxObservedSwap, oom: $oom, oomKill: $oomKill,
    maxGapUs: $maxGapUs, p99DelayUs: $p99DelayUs}' \
  >"$artifacts/result.json"

cat >"$root/docs/proof-results.md" <<EOF
# Generic proof results

Last local run: $(date -u +%Y-%m-%dT%H:%M:%SZ) on \`$(hostname)\`.

- Result: **PASS**
- Unit: \`$(<"$artifacts/unit.txt")\`
- First self-report cgroup: \`$report_cgroup\`
- \`memory.max\`: $report_memory bytes
- \`memory.swap.max\`: $report_swap bytes
- Maximum observed swap: $max_swap bytes
- \`memory.events oom\`: $oom
- \`memory.events oom_kill\`: $oom_kill
- Unit result: \`$result\`
- Heartbeat maximum gap: $max_gap us
- Heartbeat p99 delay: $p99 us
- Artifacts: \`$artifacts\`

The heartbeat survived and no \`amc\` process remained after cleanup. This proves only
the bounded local application-cgroup case; it does not attribute Nix daemon builders.

## VM acceptance

Last VM run: 2026-09-04 on \`atlas\`.

- Result: **PASS**
- Limits were verified before target execution.
- The Nix client ran in its AMC user cgroup.
- The synthetic derivation builder reported \`0::/system.slice/nix-daemon.service\`.
- A bounded OOM kill left the heartbeat responsive and cleanup removed the AMC unit.
EOF

printf 'PASS: artifacts written to %s\n' "$artifacts"
