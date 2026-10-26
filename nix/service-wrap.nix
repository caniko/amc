# Admit a program's backend through the coordinator's named contract. With
# `subcommand` set, only invocations naming it (e.g. `codex app-server`, spawned
# by an editor extension) run in the contained transient service, together with
# every child they spawn; other invocations keep direct execution.
{
  callPackage,
  lib,
  writeShellScriptBin,
  amc ? callPackage ./package.nix {},
  name,
  program,
  contract,
  subcommand ? null,
}: let
  admitted = ''exec ${amc}/bin/amc admission exec --contract ${lib.escapeShellArg contract} -- ${program} "$@"'';
in
  writeShellScriptBin name (
    if subcommand == null
    then admitted
    else ''
      for arg in "$@"; do
        if [ "$arg" = ${lib.escapeShellArg subcommand} ]; then
          ${admitted}
        fi
      done
      exec ${program} "$@"
    ''
  )
