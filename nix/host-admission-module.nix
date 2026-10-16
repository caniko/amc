{
  config,
  lib,
  pkgs,
  ...
}: let
  cfg = config.services.amc.hostAdmission;
  policy = pkgs.writeText "amc-host-policy.json" (builtins.toJSON cfg.policy);
  pagesEnabled = (cfg.policy.swap_recovery.page_cgroups or []) != [];
  preparationsEnabled = (cfg.policy.preparations or []) != [];
  guardModule = import ./page-return-guard-module.nix {inherit config lib;};
in {
  options.services.amc.hostAdmission = {
    enable = lib.mkEnableOption "root-owned shared AMC resource reservations";
    package = lib.mkOption {
      type = lib.types.package;
      default = pkgs.callPackage ./package.nix {};
    };
    policy = lib.mkOption {
      inherit ((pkgs.formats.json {})) type;
      description = "Host policy with finite enrolled domains and ceiling-backed reservations.";
    };
    healthFile = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      description = "Optional supervisor heartbeat; unavailable or inhibited evidence forbids new grants without releasing existing reservations.";
    };
  };
  config = lib.mkIf cfg.enable {
    assertions = lib.mkIf pagesEnabled guardModule.assertions;
    boot.kernelPatches = lib.mkIf pagesEnabled guardModule.boot.kernelPatches;
    environment.systemPackages = [cfg.package];
    systemd.user.slices.app-amchostrunner = lib.mkIf ((cfg.policy.namespace_runner_bytes or 0) > 0) {
      wantedBy = ["default.target"];
      sliceConfig = {
        MemoryAccounting = true;
        MemoryMax = cfg.policy.namespace_runner_bytes;
        MemorySwapMax = 0;
      };
    };
    systemd.services.amc-host-admission = {
      description = "AMC atomic host capacity reservations";
      wantedBy = ["multi-user.target"];
      serviceConfig = {
        ExecStart =
          "${lib.getExe cfg.package} admission host-serve --policy ${policy}"
          + lib.optionalString (cfg.healthFile != null) " --health-file ${lib.escapeShellArg cfg.healthFile}";
        Restart = "on-failure";
        RestartSec = "2s";
        StateDirectory = "amc-host";
        StateDirectoryMode = "0700";
        RuntimeDirectory = "amc-host";
        RuntimeDirectoryMode = "0755";
        UMask = "0077";
        MemoryMax = "128M";
        MemorySwapMax = 0;
        TasksMax = 32;
        NoNewPrivileges = true;
        ProtectSystem = "strict";
        # Preparation authorization connects back to the authenticated origin
        # under /run/user/<uid>/amc-<id>/authorize. ProtectHome=true hides that
        # socket too, causing ENOENT before the drain barrier can be installed.
        # Unix socket connections work on a read-only mount; keep both home
        # trees inaccessible and permit no runtime-file writes by the broker.
        ProtectHome =
          if preparationsEnabled
          then "read-only"
          else true;
        InaccessiblePaths = lib.optionals preparationsEnabled ["/home" "/root"];
        ProtectControlGroups = true;
        ProtectKernelTunables = true;
        RestrictAddressFamilies = ["AF_UNIX"];
      };
    };
  };
}
