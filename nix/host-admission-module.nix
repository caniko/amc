{
  config,
  lib,
  pkgs,
  ...
}: let
  cfg = config.services.amc.hostAdmission;
  policy = pkgs.writeText "amc-host-policy.json" (builtins.toJSON cfg.policy);
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
  };
  config = lib.mkIf cfg.enable {
    environment.systemPackages = [cfg.package];
    systemd.services.amc-host-admission = {
      description = "AMC atomic host capacity reservations";
      wantedBy = ["multi-user.target"];
      serviceConfig = {
        ExecStart = "${lib.getExe cfg.package} admission host-serve --policy ${policy}";
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
        ProtectHome = true;
        ProtectControlGroups = true;
        ProtectKernelTunables = true;
        RestrictAddressFamilies = ["AF_UNIX"];
      };
    };
  };
}
