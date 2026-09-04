{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.programs.amc;
  format = pkgs.formats.toml { };
in
{
  options.programs.amc = {
    enable = lib.mkEnableOption "Application Memory Contracts";

    package = lib.mkOption {
      type = lib.types.package;
      default = pkgs.callPackage ./package.nix { };
      defaultText = lib.literalExpression "pkgs.callPackage ./nix/package.nix { }";
      description = "The amc package to install.";
    };

    settings = lib.mkOption {
      inherit (format) type;
      default = {
        version = 1;
        profiles = { };
        applications = { };
      };
      example = lib.literalExpression ''
        {
          version = 1;
          profiles.interactive = {
            slice = "app-amc.slice";
            memory_max = "12GiB";
            memory_swap_max = "1GiB";
          };
          applications."ai.opencode".profile = "interactive";
        }
      '';
      description = ''
        Configuration written to /etc/xdg/amc/config.toml. The example limits
        are illustrative; replace them using measurements from your own RAM
        and workload before enabling an application contract.
      '';
    };

    createSlices = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = "Whether to create app-amc.slice and background-amc.slice for each user manager.";
    };

    nixBuildPool = {
      enable = lib.mkEnableOption "the experimental global Nix build-pool memory bridge";

      memoryMax = lib.mkOption {
        type = lib.types.str;
        default = "24GiB";
        description = "Aggregate Nix build-pool memory limit, reserved for the blocked bridge.";
      };

      memorySwapMax = lib.mkOption {
        type = lib.types.str;
        default = "2GiB";
        description = "Aggregate Nix build-pool swap limit, reserved for the blocked bridge.";
      };
    };
  };

  config = lib.mkMerge [
    {
      assertions = [
        {
          assertion = !cfg.nixBuildPool.enable;
          message = ''
            programs.amc.nixBuildPool is blocked in v0.1 until the generic
            containment proof and a separate Nix daemon recovery VM test pass.
          '';
        }
      ];
    }

    (lib.mkIf cfg.enable {
      environment.etc."xdg/amc/config.toml".source = format.generate "amc-config.toml" cfg.settings;
      environment.systemPackages = [ cfg.package ];

      systemd.user.slices = lib.mkIf cfg.createSlices {
        app-amc = {
          description = "Application Memory Contracts interactive workloads";
          sliceConfig.MemoryAccounting = true;
        };
        background-amc = {
          description = "Application Memory Contracts background workloads";
          sliceConfig.MemoryAccounting = true;
        };
      };
    })
  ];
}
