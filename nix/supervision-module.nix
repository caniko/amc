{
  config,
  lib,
  pkgs,
  ...
}: let
  cfg = config.services.amc.supervision;
  policy = pkgs.writeText "amc-supervision-policy.json" (builtins.toJSON cfg.policy);
in {
  options.services.amc.supervision = {
    enable = lib.mkEnableOption "bounded host-wide AMC native-domain supervision";
    package = lib.mkOption {
      type = lib.types.package;
      default = pkgs.callPackage ./package.nix {};
    };
    policy = lib.mkOption {
      inherit ((pkgs.formats.json {})) type;
      description = "Explicit shadow/enforce policy, enrolled native lifecycle authority and durable recovery budgets. Admission is configured independently.";
    };
  };
  config = lib.mkIf cfg.enable {
    environment.systemPackages = [cfg.package];
    systemd.services.amc-supervision = {
      description = "AMC bounded native memory supervision";
      wantedBy = ["multi-user.target"];
      path = [pkgs.systemd];
      serviceConfig = {
        ExecStart = "${lib.getExe cfg.package} supervise serve --policy ${policy} --systemctl ${pkgs.systemd}/bin/systemctl";
        Restart = "on-failure";
        RestartSec = "2s";
        StateDirectory = "amc-supervision";
        StateDirectoryMode = "0700";
        RuntimeDirectory = "amc-supervision";
        RuntimeDirectoryMode = "0755";
        RuntimeDirectoryPreserve = "yes";
        UMask = "0077";
        MemoryMax = "512M";
        MemorySwapMax = 0;
        TasksMax = 32;
        LimitNOFILE = 32768;
        NoNewPrivileges = true;
        ProtectSystem = "strict";
        ProtectHome = "read-only";
        ProtectControlGroups = true;
        ProtectKernelTunables = true;
        RestrictAddressFamilies = ["AF_UNIX"];
        KillMode = "control-group";
        TimeoutStopSec = "5s";
      };
    };
  };
}
