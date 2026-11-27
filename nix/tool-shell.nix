# Agent tool shell. Agents such as OpenCode execute every tool command as
# `<shell> -c <command>`; pointing their shell setting at this shim starts each
# command in a hard-contained transient service before execution, with
# argv/cwd/env/stdio/exit status preserved and cancellation stopping the whole
# unit. Other invocations pass through to the real shell. The consumer owns the
# slice, limits and admission contract.
{
  bash,
  callPackage,
  lib,
  writeShellScriptBin,
  amc ? callPackage ./package.nix { },
  name ? "amc-tool-shell",
  slice ? null,
  memoryMaxBytes ? null,
  memorySwapMaxBytes ? null,
  # Optional attrset `{ slice, memory_max, memory_swap_max }` passed directly
  # to `executionArgs` in place of separate slice/memoryMaxBytes/memorySwapMaxBytes.
  nativeContract ? null,
  # Route through the admission coordinator's named contract instead of the
  # direct hard-limited unit above.
  admission ? false,
  contract ? "tool",
  # Optional consumer command that takes over `-c` invocations entirely; it is
  # exec'd as `<hook> [hookArgs...] -c <command>` and owns containment itself.
  hook ? null,
  hookArgs ? [ ],
  realShell ? "${bash}/bin/bash",
  runtimeMaxSec ? 1800,
  # Queueing capability advertisement for consumers (e.g. OpenCode v2 plugin).
  queue ? false,
  admissionReadyVersion ? 2,
  admissionBackpressureVersion ? 1,
  shellAdmissionReadyVersion ? (if queue then admissionReadyVersion else 0),
  shellAdmissionBackpressureVersion ? (if queue then admissionBackpressureVersion else 0),
}:
let
  executionArgs = import ./execution-args.nix;
  effectiveContract =
    if nativeContract != null then
      nativeContract
    else
      {
        inherit slice;
        memory_max = memoryMaxBytes;
        memory_swap_max = memorySwapMaxBytes;
      };
in
assert runtimeMaxSec >= 1 && runtimeMaxSec <= 86400;
assert
  hook != null
  || admission
  || (
    effectiveContract.slice != null
    && effectiveContract.memory_max != null
    && effectiveContract.memory_swap_max != null
  );
(writeShellScriptBin name ''
  if [ "''${1:-}" = "-c" ]; then
    ${
      if hook != null then
        ''exec ${hook}${lib.optionalString (hookArgs != [ ]) " ${lib.escapeShellArgs hookArgs}"} "$@"''
      else
        ''
          exec ${amc}/bin/amc \
            ${
              lib.escapeShellArgs (executionArgs {
                inherit admission runtimeMaxSec;
                contractName = contract;
                contract = effectiveContract;
              })
            } -- ${realShell} "$@"
        ''
    }
  fi
  exec ${realShell} "$@"
'').overrideAttrs
  {
    passthru = {
      inherit shellAdmissionReadyVersion shellAdmissionBackpressureVersion;
    };
  }
