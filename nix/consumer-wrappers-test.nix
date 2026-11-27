# Routing check for the consumer shims against a fake amc that prints its argv.
{
  pkgs,
  mkToolShell,
  mkServiceWrap,
  mkServiceExec,
  mkCodexServiceWrap,
}:
let
  amc = pkgs.writeShellScriptBin "amc" ''printf 'amc %s\n' "$*"'';
  program = pkgs.writeShellScriptBin "prog" ''printf 'prog %s\n' "$*"'';
  codex = pkgs.writeShellScriptBin "codex" ''printf 'codex %s\n' "$*"'';
  serviceExec = mkServiceExec pkgs { inherit amc; };
  direct = mkToolShell pkgs {
    inherit amc;
    slice = "tools.slice";
    memoryMaxBytes = 64;
    memorySwapMaxBytes = 0;
    runtimeMaxSec = 30;
  };
  admitted = direct.override {
    name = "admitted-shell";
    admission = true;
  };
  queued = direct.override {
    name = "queued-shell";
    hook = "${amc}/bin/amc";
    hookArgs = [
      "hook"
      "--queue"
      "--"
    ];
    queue = true;
  };
  hooked = direct.override {
    name = "hooked-shell";
    hook = "${amc}/bin/amc hook --";
  };
  wrap = mkServiceWrap pkgs {
    inherit amc;
    name = "prog-wrap";
    program = "${program}/bin/prog";
    contract = "backend";
    subcommand = "serve";
  };
  wrapNative = mkServiceWrap pkgs {
    inherit amc;
    name = "prog-native-wrap";
    program = "${program}/bin/prog";
    nativeContract = {
      slice = "backend.slice";
      memory_max = 128;
      memory_swap_max = 0;
    };
    runtimeMaxSec = 3600;
    subcommand = "serve";
  };
  codexAdmitted = mkCodexServiceWrap pkgs {
    inherit amc codex;
    amcServiceExec = serviceExec;
  };
  codexNative = mkCodexServiceWrap pkgs {
    inherit amc codex;
    nativeContract = {
      slice = "codex.slice";
      memory_max = 256;
      memory_swap_max = 0;
    };
  };
in
assert direct.shellAdmissionReadyVersion == 0;
assert direct.shellAdmissionBackpressureVersion == 0;
assert queued.shellAdmissionReadyVersion == 2;
assert queued.shellAdmissionBackpressureVersion == 1;
pkgs.runCommand "amc-consumer-wrappers" { } ''
  set -eu
  expect() { [ "$1" = "$2" ] || { printf 'expected: %s\ngot:      %s\n' "$2" "$1" >&2; exit 1; }; }
  expect "$(${serviceExec}/bin/amc-service-exec --contract demo -- true)" \
    "amc admission exec --contract demo -- true"
  expect "$(${direct}/bin/amc-tool-shell -c 'echo hi')" \
    "amc exec --slice tools.slice --memory-max 64 --memory-swap-max 0 --runtime-max-sec 30 -- ${pkgs.bash}/bin/bash -c echo hi"
  expect "$(${direct}/bin/amc-tool-shell -s <<< 'echo passthrough')" "passthrough"
  expect "$(${admitted}/bin/admitted-shell -c true)" \
    "amc admission exec --contract tool --runtime-max-sec 30 -- ${pkgs.bash}/bin/bash -c true"
  expect "$(${hooked}/bin/hooked-shell -c true)" "amc hook -- -c true"
  expect "$(${queued}/bin/queued-shell -c true)" "amc hook --queue -- -c true"
  expect "$(${wrap}/bin/prog-wrap --flag serve)" \
    "amc admission exec --contract backend -- ${program}/bin/prog --flag serve"
  expect "$(${wrap}/bin/prog-wrap --version)" "prog --version"
  expect "$(${wrapNative}/bin/prog-native-wrap --flag serve)" \
    "amc exec --slice backend.slice --memory-max 128 --memory-swap-max 0 --runtime-max-sec 3600 -- ${program}/bin/prog --flag serve"
  expect "$(${wrapNative}/bin/prog-native-wrap --version)" "prog --version"
  expect "$(${codexAdmitted}/bin/codex-service-wrap app-server --stdio)" \
    "amc admission exec --contract codex -- ${codex}/bin/codex app-server --stdio"
  expect "$(${codexAdmitted}/bin/codex-service-wrap --version)" "codex --version"
  expect "$(${codexNative}/bin/codex-service-wrap app-server --stdio)" \
    "amc exec --slice codex.slice --memory-max 256 --memory-swap-max 0 --runtime-max-sec 86400 -- ${codex}/bin/codex app-server --stdio"
  touch $out
''
