{
  config,
  lib,
  pkgs,
  ...
}: let
  cfg = config.programs.amc;
  format = pkgs.formats.toml {};
in {
  options.programs.amc = {
    enable = lib.mkEnableOption "Application Memory Contracts (disposable fixture harness; real applications use native systemd units/drop-ins)";

    package = lib.mkOption {
      type = lib.types.package;
      default = pkgs.callPackage ./package.nix {};
      defaultText = lib.literalExpression "pkgs.callPackage ./nix/package.nix { }";
      description = "The amc package to install.";
    };

    settings = lib.mkOption {
      inherit (format) type;
      default = {
        version = 1;
        profiles = {};
        applications = {};
      };
      example = lib.literalExpression ''
        {
          version = 1;
          profiles.proof = {
            slice = "app-amc.slice";
            memory_max = "256MiB";
            memory_swap_max = "0B";
          };
          applications."amc.proof".profile = "proof";
        }
      '';
      description = ''
        Fixture/compatibility configuration written to
        /etc/xdg/amc/config.toml for amc run/launch test fixtures. The
        example limits are illustrative fixture numbers, not production
        policy; configure real applications with native systemd
        units/drop-ins (see docs/rfc-v0.4/nixos-opencode.md). Supports an
        optional per-profile memory_high (native MemoryHigh); when absent,
        MemoryHigh is not emitted.
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
      environment.systemPackages = [cfg.package];

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
