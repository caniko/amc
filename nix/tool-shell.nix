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
  amc ? callPackage ./package.nix {},
  name ? "amc-tool-shell",
  slice,
  memoryMaxBytes,
  memorySwapMaxBytes,
  # Route through the admission coordinator's named contract instead of the
  # direct hard-limited unit above.
  admission ? false,
  contract ? "tool",
  # Optional consumer command that takes over `-c` invocations entirely; it is
  # exec'd as `<hook> -c <command>` and owns containment itself.
  hook ? null,
  realShell ? "${bash}/bin/bash",
  runtimeMaxSec ? 1800,
}:
assert runtimeMaxSec >= 1 && runtimeMaxSec <= 86400;
  writeShellScriptBin name ''
    if [ "''${1:-}" = "-c" ]; then
      ${
      if hook != null
      then ''exec ${hook} "$@"''
      else ''
        exec ${amc}/bin/amc \
          ${
          if admission
          then "admission exec --contract ${lib.escapeShellArg contract}"
          else "exec --slice ${lib.escapeShellArg slice} --memory-max ${toString memoryMaxBytes} --memory-swap-max ${toString memorySwapMaxBytes}"
        } \
          --runtime-max-sec ${toString runtimeMaxSec} -- ${realShell} "$@"
      ''
    }
    fi
    exec ${realShell} "$@"
  ''
