{
  lib,
  pkgs,
  nixosModule,
}:
let
  eval =
    module:
    (lib.evalModules {
      modules = [
        {
          options.systemd.user.services = lib.mkOption {
            type = lib.types.attrsOf lib.types.anything;
            default = { };
          };
          config._module.args = { inherit pkgs; };
        }
        nixosModule
        module
      ];
    }).config;
  disabled = eval {
    services.amc.observeUnits.enable = false;
  };
  enabled = eval {
    services.amc.observeUnits = {
      enable = true;
      policyRev = "test-observe-1";
      unit = "background-quiesce.service";
      entryPoint = "gamemode-hook";
      host = "test-host";
      extraArgs = [
        "--threshold-bytes"
        "4294967296"
      ];
    };
  };
  enter = enabled.systemd.user.services."amc-observe-enter";
  exit = enabled.systemd.user.services."amc-observe-exit";
in
assert !(disabled.systemd.user.services ? "amc-observe-enter");
assert !(disabled.systemd.user.services ? "amc-observe-exit");
assert enter.serviceConfig.Type == "oneshot";
assert enter.serviceConfig.TimeoutStartSec == "30s";
assert enter.serviceConfig.KillMode == "control-group";
assert enter.serviceConfig.StandardOutput == "journal";
assert enter.serviceConfig.StandardError == "journal";
assert enter.serviceConfig.SyslogIdentifier == "amc-headroom";
assert lib.hasInfix "--policy-rev test-observe-1" enter.serviceConfig.ExecStart;
assert lib.hasInfix "--unit background-quiesce.service" enter.serviceConfig.ExecStart;
assert lib.hasInfix "--entry-point gamemode-hook" enter.serviceConfig.ExecStart;
assert lib.hasInfix "--event memory.observed" enter.serviceConfig.ExecStart;
assert lib.hasInfix "--action quiescence-enter-assessment" enter.serviceConfig.ExecStart;
assert lib.hasInfix "--reason gamemode-start" enter.serviceConfig.ExecStart;
assert lib.hasInfix "--host test-host" enter.serviceConfig.ExecStart;
assert lib.hasInfix "--threshold-bytes 4294967296" enter.serviceConfig.ExecStart;
assert exit.serviceConfig.Type == "oneshot";
assert lib.hasInfix "--action quiescence-exit-assessment" exit.serviceConfig.ExecStart;
assert lib.hasInfix "--reason gamemode-end" exit.serviceConfig.ExecStart;
pkgs.runCommand "amc-observe-units-module-eval" { } "touch $out"
