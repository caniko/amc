{
  description = "Application Memory Contracts";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = {
    self,
    nixpkgs,
  }: let
    systems = [
      "aarch64-linux"
      "x86_64-linux"
    ];
    forAllSystems = nixpkgs.lib.genAttrs systems;
    pkgsFor = system: import nixpkgs {inherit system;};
  in {
    packages = forAllSystems (system: {
      default = (pkgsFor system).callPackage ./nix/package.nix {};
    });

    apps = forAllSystems (system: {
      default = {
        type = "app";
        program = "${self.packages.${system}.default}/bin/amc";
      };
    });

    checks = forAllSystems (
      system: let
        pkgs = pkgsFor system;
        package = self.packages.${system}.default;
        mkCargoCheck = name: nativeBuildInputs: command:
          package.overrideAttrs (old: {
            pname = "amc-${name}";
            nativeBuildInputs = old.nativeBuildInputs ++ nativeBuildInputs;
            buildPhase = command;
            checkPhase = "true";
            installPhase = "touch $out";
            # Checks produce a marker, not the installed CLI executable.
            postInstall = "";
            postFixup = "";
          });
      in {
        fmt = mkCargoCheck "fmt" [pkgs.rustfmt] "cargo fmt --all --check";
        clippy = mkCargoCheck "clippy" [pkgs.clippy] "cargo clippy --workspace --all-targets --locked -- -D warnings";
        test = mkCargoCheck "test" [] "cargo test --workspace --all-targets --locked";
        admission-features = mkCargoCheck "admission-features" [pkgs.clippy] ''
          set -e
          for features in default none sync async sysinfo systemd; do
            flags=()
            if [ "$features" != default ]; then
              flags+=(--no-default-features)
              if [ "$features" != none ]; then
                flags+=(--features "$features")
              fi
            fi
            cargo test -p amc-runner --locked "''${flags[@]}"
            cargo clippy -p amc-runner --all-targets --locked "''${flags[@]}" -- -D warnings
            RUSTDOCFLAGS="-D warnings" cargo doc -p amc-runner --no-deps --locked "''${flags[@]}"
          done
        '';
        fixture-scripts =
          pkgs.runCommand "amc-fixture-scripts" {
            nativeBuildInputs = [pkgs.python3 pkgs.bash];
          } ''
            python3 ${self}/scripts/check-fixtures.py
            bash -n ${self}/scripts/prove-local.sh
            touch $out
          '';
      }
    );

    devShells = forAllSystems (
      system: let
        pkgs = pkgsFor system;
      in {
        default = pkgs.mkShell {
          inputsFrom = [self.packages.${system}.default];
          packages = with pkgs; [
            bash
            cargo
            clippy
            jq
            python3
            rust-analyzer
            rustc
            rustfmt
          ];
        };
      }
    );

    formatter = forAllSystems (system: (pkgsFor system).nixfmt-tree);

    nixosModules.default = import ./nix/module.nix;
    nixosModules.host-admission = import ./nix/host-admission-module.nix;
    lib.hostDomainPressureVersion = 1;
    lib.admissionSizingVersion = 1;
    lib.admissionBurstVersion = 1;
    lib.admissionPreparationVersion = 2;
    lib.admissionContinuationEnvelopesVersion = 1;
    lib.swapReturnReservationVersion = 2;
    lib.swapRestorationManifestVersion = 1;
    nixosModules.supervision = import ./nix/supervision-module.nix;
    lib.supervisionPolicyVersion = 1;
    homeManagerModules.default = import ./nix/home-module.nix;

    # Deliberately excluded from checks: run explicitly with
    # nix build .#nixosTests.x86_64-linux.generic
    nixosTests = forAllSystems (
      system: let
        pkgs = pkgsFor system;
      in {
        supervision = import ./nix/supervision-vm-test.nix {
          inherit pkgs;
          package = self.packages.${system}.default;
          module = self.nixosModules.supervision;
        };
        shared-admission = import ./nix/host-admission-vm-test.nix {
          inherit pkgs;
          package = self.packages.${system}.default;
        };
        admission = import ./nix/admission-vm-test.nix {
          inherit pkgs;
          amcPackage = self.packages.${system}.default;
        };
        generic = import ./nix/vm-test.nix {
          inherit pkgs;
          amcModule = self.nixosModules.default;
          amcPackage = self.packages.${system}.default;
        };
      }
    );
  };
}
