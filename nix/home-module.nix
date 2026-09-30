{
  config,
  lib,
  pkgs,
  ...
}: let
  cfg = config.services.amc.admission;
  policy = pkgs.writeText "amc-admission-policy.json" (builtins.toJSON cfg.policy);
  slices = lib.unique (map (contract: contract.slice) (lib.attrValues (cfg.policy.contracts or {})));
in {
  options.services.amc.admission = {
    enable = lib.mkEnableOption "durable per-user AMC admission for native workloads";
    package = lib.mkOption {
      type = lib.types.package;
      default = pkgs.callPackage ./package.nix {};
      description = "AMC package with the admission server and client.";
    };
    policy = lib.mkOption {
      type = (pkgs.formats.json {}).type;
      description = "Version 1 admission policy. Native slice limits must be declared by the consumer.";
    };
  };
  config = lib.mkIf cfg.enable {
    home.packages = [cfg.package];
    xdg.configFile."amc/admission.json".source = policy;
    systemd.user.services.amc-admission = {
      Unit = {
        Description = "AMC durable per-user memory admission";
        Requires = slices;
        After = slices;
      };
      Service = {
        ExecStart = "${lib.getExe cfg.package} admission serve --policy ${policy} --systemctl ${pkgs.systemd}/bin/systemctl";
        Environment = ["PATH=${lib.makeBinPath [pkgs.systemd]}"];
        UMask = "0077";
        Restart = "on-failure";
        RestartSec = "2s";
        MemoryAccounting = true;
        MemoryMax = "128M";
        MemorySwapMax = 0;
        TasksMax = 32;
        NoNewPrivileges = true;
        Slice = "app-amc.slice";
      };
      Install.WantedBy = ["default.target"];
    };
  };
}
