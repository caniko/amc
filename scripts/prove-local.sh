#!/usr/bin/env bash
# Passive diagnostics only. `amc doctor` is separate: it starts/stops a probe.
set -euo pipefail
if [[ ${1:-} == --host-stress ]]; then
  printf '%s\n' 'Host stress is disabled. Use the explicit disposable VM target.' >&2
  exit 2
fi
if (( $# != 1 )) || [[ $1 == -* ]]; then
  printf '%s\n' 'Usage: prove-local.sh UNIT.service (no launches, no stress)' >&2
  exit 2
fi
root=$(dirname -- "$(dirname -- "$(realpath -- "${BASH_SOURCE[0]}")")")
amc=${AMC_BIN:-"$root/target/debug/amc"}
if [[ ! -x $amc ]]; then
  printf '%s\n' 'Build AMC in the pinned development environment first.' >&2
  exit 1
fi
umask 077
mkdir -p "$root/proof-artifacts"
artifacts=$(mktemp -d "$root/proof-artifacts/inspect-XXXXXXXX")
"$amc" inspect "$1" --json >"$artifacts/inspection.json"
printf 'Inspection artifact (not a proof result): %s\n' "$artifacts/inspection.json"
