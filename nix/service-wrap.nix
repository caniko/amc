# Admit a program's backend through the coordinator's named contract, or run it
# under a direct native AMC contract (`nativeContract` + `runtimeMaxSec`). With
# `subcommand` set, only invocations naming it (e.g. `codex app-server`, spawned
# by an editor extension) run in the contained transient service, together with
# every child they spawn; other invocations keep direct execution. Any argv token
# matches: callers put valued global options first (`codex -c k=v app-server`),
# so a positional match needs per-program option arity. An unrelated argument
# equal to the subcommand is admitted too, which contains it but cannot escape.
{
  callPackage,
  lib,
  writeShellScriptBin,
  amc ? callPackage ./package.nix { },
  amcServiceExec ? null,
  name,
  program,
  contract ? null,
  nativeContract ? null,
  runtimeMaxSec ? 86400,
  subcommand ? null,
}:
assert nativeContract != null || (contract != null && contract != "");
let
  executionArgs = import ./execution-args.nix;
  admitted =
    if nativeContract != null then
      ''exec ${amc}/bin/amc ${
        lib.escapeShellArgs (executionArgs {
          admission = false;
          contract = nativeContract;
          inherit runtimeMaxSec;
        })
      } -- ${program} "$@"''
    else if amcServiceExec != null then
      ''exec ${amcServiceExec}/bin/amc-service-exec --contract ${lib.escapeShellArg contract} -- ${program} "$@"''
    else
      ''exec ${amc}/bin/amc admission exec --contract ${lib.escapeShellArg contract} -- ${program} "$@"'';
in
writeShellScriptBin name (
  if subcommand == null then
    admitted
  else
    ''
      for arg in "$@"; do
        if [ "$arg" = ${lib.escapeShellArg subcommand} ]; then
          ${admitted}
        fi
      done
      exec ${program} "$@"
    ''
)
