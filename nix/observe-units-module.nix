# Shared memory headroom observer user units (enter/exit): short-lived oneshots
# that run only the memory observer with fixed, allowlisted arguments. systemd
# owns timeout, control-group cleanup, and journal delivery; no custom
# supervisor, no limits, no service control.
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.amc.observeUnits;
  mkObserveUnit = phase: reason: action: {
    description = "AMC memory headroom observation (${phase})";
    serviceConfig = {
      Type = "oneshot";
      ExecStart = lib.escapeShellArgs (
        [
          (lib.getExe cfg.package)
          "--policy-rev"
          cfg.policyRev
          "--unit"
          cfg.unit
          "--entry-point"
          cfg.entryPoint
          "--event"
          "memory.observed"
          "--action"
          action
          "--reason"
          reason
        ]
        ++ lib.optionals (cfg.host != null) [
          "--host"
          cfg.host
        ]
        ++ cfg.extraArgs
      );
      TimeoutStartSec = "30s";
      KillMode = "control-group";
      StandardOutput = "journal";
      StandardError = "journal";
      SyslogIdentifier = "amc-headroom";
    };
  };
in
{
  options.services.amc.observeUnits = {
    enable = lib.mkEnableOption "AMC memory headroom observation user units";

    package = lib.mkOption {
      type = lib.types.package;
      default = pkgs.callPackage ./headroom-observe.nix { };
      defaultText = lib.literalExpression "pkgs.callPackage ./nix/headroom-observe.nix {}";
      description = "The amc-headroom-observe package providing amc-headroom-assess.";
    };

    policyRev = lib.mkOption {
      type = lib.types.str;
      description = "Policy revision identifier recorded in emitted observation events.";
    };

    unit = lib.mkOption {
      type = lib.types.str;
      description = "Unit label recorded in emitted observation events.";
    };

    entryPoint = lib.mkOption {
      type = lib.types.enum [
        "gamemode-hook"
        "operator-cli"
        "scheduled"
        "worker"
      ];
      default = "gamemode-hook";
      description = "Entry point recorded in emitted observation events.";
    };

    host = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      description = "Optional host name override passed via --host.";
    };

    extraArgs = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      description = "Additional allowlisted arguments appended to amc-headroom-assess.";
    };
  };

  config = lib.mkIf cfg.enable {
    systemd.user.services."amc-observe-enter" =
      mkObserveUnit "enter" "gamemode-start"
        "quiescence-enter-assessment";
    systemd.user.services."amc-observe-exit" =
      mkObserveUnit "exit" "gamemode-end"
        "quiescence-exit-assessment";
  };
}
