# Routing check for the consumer shims against a fake amc that prints its argv.
{
  pkgs,
  mkToolShell,
  mkServiceWrap,
}: let
  amc = pkgs.writeShellScriptBin "amc" ''printf 'amc %s\n' "$*"'';
  program = pkgs.writeShellScriptBin "prog" ''printf 'prog %s\n' "$*"'';
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
in
  pkgs.runCommand "amc-consumer-wrappers" {} ''
    set -eu
    expect() { [ "$1" = "$2" ] || { printf 'expected: %s\ngot:      %s\n' "$2" "$1" >&2; exit 1; }; }
    expect "$(${direct}/bin/amc-tool-shell -c 'echo hi')" \
      "amc exec --slice tools.slice --memory-max 64 --memory-swap-max 0 --runtime-max-sec 30 -- ${pkgs.bash}/bin/bash -c echo hi"
    expect "$(${direct}/bin/amc-tool-shell -s <<< 'echo passthrough')" "passthrough"
    expect "$(${admitted}/bin/admitted-shell -c true)" \
      "amc admission exec --contract tool --runtime-max-sec 30 -- ${pkgs.bash}/bin/bash -c true"
    expect "$(${hooked}/bin/hooked-shell -c true)" "amc hook -- -c true"
    expect "$(${wrap}/bin/prog-wrap --flag serve)" \
      "amc admission exec --contract backend -- ${program}/bin/prog --flag serve"
    expect "$(${wrap}/bin/prog-wrap --version)" "prog --version"
    touch $out
  ''
