{
  description = "Application Memory Contracts";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  # Exact Canix Atlas kernel source; native CI uses its baseline CPU variant.
  inputs.atlas-kernel.url = "github:xddxdd/nix-cachyos-kernel/b21e2ad2d1a0e031e6fe7f02a8bdaf010f922887";

  outputs =
    {
      self,
      nixpkgs,
      atlas-kernel,
    }:
    let
      systems = [
        "aarch64-linux"
        "x86_64-linux"
      ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
      pkgsFor = system: import nixpkgs { inherit system; };
    in
    {
      nixosModules.page-return-guard = import ./nix/page-return-guard-module.nix;
      nixosModules.observe-units = import ./nix/observe-units-module.nix;

      packages = forAllSystems (
        system:
        let
          pkgs = pkgsFor system;
        in
        {
          default = pkgs.callPackage ./nix/package.nix { };
          amc-service-exec = pkgs.callPackage ./nix/service-exec.nix {
            amc = self.packages.${system}.default;
          };
          amc-headroom-observe = pkgs.callPackage ./nix/headroom-observe.nix { };
          opencode-amc-resources-v2 = pkgs.runCommand "opencode-amc-resources-v2" { } ''
            mkdir -p "$out"
            cp ${./plugins/opencode/amc-resources-v2/server.js} "$out/server.js"
          '';
          opencode-amc-status-v2 = pkgs.runCommand "opencode-amc-status-v2" { } ''
            mkdir -p "$out"
            cp ${./plugins/opencode/amc-status-v2/server.js} "$out/server.js"
          '';
        }
      );

      apps = forAllSystems (system: {
        default = {
          type = "app";
          program = "${self.packages.${system}.default}/bin/amc";
        };
      });

      checks = forAllSystems (
        system:
        let
          pkgs = pkgsFor system;
          package = self.packages.${system}.default;
          mkCargoCheck =
            name: nativeBuildInputs: command:
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
        in
        {
          fmt = mkCargoCheck "fmt" [ pkgs.rustfmt ] "cargo fmt --all --check";
          clippy = mkCargoCheck "clippy" [
            pkgs.clippy
          ] "cargo clippy --workspace --all-targets --locked -- -D warnings";
          test = mkCargoCheck "test" [ ] "cargo test --workspace --all-targets --locked";
          admission-features = mkCargoCheck "admission-features" [ pkgs.clippy ] ''
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
          consumer-wrappers = import ./nix/consumer-wrappers-test.nix {
            inherit pkgs;
            inherit (self.lib)
              mkToolShell
              mkServiceWrap
              mkServiceExec
              mkCodexServiceWrap
              ;
          };
          headroom-observe-policy-events =
            pkgs.runCommand "amc-headroom-observe-policy-events"
              {
                nativeBuildInputs = [
                  pkgs.gnugrep
                  pkgs.python3
                ];
              }
              ''
                set -euo pipefail
                bin="${nixpkgs.lib.getExe self.packages.${system}.amc-headroom-observe}"
                checker="${self}/scripts/amc-headroom-policy-events-check.py"
                python3 "$checker"
                "$bin" --self-test
                export PROC_MEMINFO="$TMPDIR/meminfo" PROC_PRESSURE_MEMORY="$TMPDIR/pressure"
                printf 'MemTotal: 1 kB\nMemAvailable: 8000 kB\n' > "$TMPDIR/meminfo"
                printf 'some avg10=1.00 avg300=2.00 total=3\nfull avg10=4.00 avg300=5.00 total=0\n' > "$TMPDIR/pressure"
                "$bin" --policy-rev check --unit check.service --entry-point gamemode-hook \
                  --event memory.observed --action quiescence-enter-assessment \
                  --reason gamemode-start > event.jsonl
                python3 "$checker" event.jsonl
                grep -q '"outcome": "ok"' event.jsonl
                PROC_MEMINFO=/nonexistent "$bin" --policy-rev check --unit check.service > unk.jsonl
                python3 "$checker" unk.jsonl
                grep -q '"outcome": "unknown"' unk.jsonl
                if "$bin" --policy-rev x --unit u --threshold-bytes 0 2>/dev/null; then
                  echo "invalid threshold accepted" >&2
                  exit 1
                fi
                touch "$out"
              '';
          observe-units-module-eval = import ./nix/test-observe-units-module.nix {
            inherit pkgs;
            inherit (nixpkgs) lib;
            nixosModule = self.nixosModules.observe-units;
          };
          opencode-amc-resources-v2 =
            pkgs.runCommand "amc-opencode-amc-resources-v2"
              {
                nativeBuildInputs = [ pkgs.nodejs ];
              }
              ''
                set -euo pipefail
                test -f ${self.packages.${system}.opencode-amc-resources-v2}/server.js
                node --test ${self}/plugins/opencode/test-amc-resources-v2.mjs
                touch "$out"
              '';
          opencode-amc-status-v2 =
            pkgs.runCommand "amc-opencode-amc-status-v2"
              {
                nativeBuildInputs = [ pkgs.nodejs ];
              }
              ''
                set -euo pipefail
                test -f ${self.packages.${system}.opencode-amc-status-v2}/server.js
                node --test ${self}/plugins/opencode/test-amc-status-v2.mjs
                touch "$out"
              '';
          fixture-scripts =
            pkgs.runCommand "amc-fixture-scripts"
              {
                nativeBuildInputs = [
                  pkgs.python3
                  pkgs.bash
                ];
              }
              ''
                python3 ${self}/scripts/check-fixtures.py
                bash -n ${self}/scripts/prove-local.sh
                touch $out
              '';
          # The header tests run here; the patched Nix is only evaluated, its
          # build is the consumer's gate.
          execution-policy =
            assert import ./nix/test-execution-policy.nix;
            assert (self.lib.mkNixBuilderPackage { inherit pkgs; }).drvPath != "";
            pkgs.runCommand "amc-execution-policy"
              {
                buildInputs = [ pkgs.nlohmann_json ];
                nativeBuildInputs = [ pkgs.stdenv.cc ];
              }
              ''
                for test in policy host-admission; do
                  c++ -std=c++20 -Wall -Wextra -Werror -I ${./nix/nix-builder} \
                    ${./nix/nix-builder}/$test.cc -o $test
                done
                ./policy "$PWD/slot"
                ./host-admission
                touch $out
              '';
        }
      );

      devShells = forAllSystems (
        system:
        let
          pkgs = pkgsFor system;
        in
        {
          default = pkgs.mkShell {
            inputsFrom = [ self.packages.${system}.default ];
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
      lib.executionPolicyVersion = 1;
      lib.executionArgs = import ./nix/execution-args.nix;
      lib.nixBuilderSettings = import ./nix/nix-builder-settings.nix;
      lib.mkNixBuilderPackage = import ./nix/nix-builder/package.nix;
      lib.admissionSizingVersion = 1;
      lib.admissionQueueVersion = 1;
      lib.admissionReadyVersion = 2;
      lib.admissionBackpressureVersion = 1;
      lib.admissionProtectedPoolVersion = 1;
      lib.admissionBurstVersion = 1;
      lib.admissionPreparationVersion = 2;
      lib.admissionContinuationEnvelopesVersion = 1;
      lib.admissionNamespaceRunnerVersion = 2;
      lib.swapReturnReservationVersion = 2;
      lib.swapRestorationManifestVersion = 1;
      lib.pageReturnKernelGuardVersion = 1;
      nixosModules.supervision = import ./nix/supervision-module.nix;
      lib.supervisionPolicyVersion = 1;
      homeManagerModules.default = import ./nix/home-module.nix;
      # Consumer launch shims: `inputs.amc.lib.mkToolShell pkgs { slice = ...; }`.
      lib.mkServiceExec = pkgs: pkgs.callPackage ./nix/service-exec.nix;
      lib.mkToolShell =
        pkgs: args:
        pkgs.callPackage ./nix/tool-shell.nix (
          {
            inherit (self.lib) admissionReadyVersion admissionBackpressureVersion;
          }
          // args
        );
      lib.mkServiceWrap = pkgs: pkgs.callPackage ./nix/service-wrap.nix;
      lib.mkCodexServiceWrap =
        pkgs:
        {
          codex,
          name ? "codex-service-wrap",
          contract ? "codex",
          nativeContract ? null,
          runtimeMaxSec ? 86400,
          subcommand ? "app-server",
          ...
        }@args:
        self.lib.mkServiceWrap pkgs (
          (builtins.removeAttrs args [ "codex" ])
          // {
            inherit
              name
              contract
              nativeContract
              runtimeMaxSec
              subcommand
              ;
            program = "${codex}/bin/codex";
          }
        );

      # Deliberately excluded from checks: run explicitly with
      # nix build .#nixosTests.x86_64-linux.generic
      nixosTests = forAllSystems (
        system:
        let
          pkgs = pkgsFor system;
        in
        {
          protected-pool = import ./nix/protected-pool-vm-test.nix {
            inherit pkgs;
            package = self.packages.${system}.default;
          };
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
        // nixpkgs.lib.optionalAttrs (system == "x86_64-linux") {
          shared-admission-atlas-kernel = import ./nix/host-admission-vm-test.nix {
            inherit pkgs;
            package = self.packages.${system}.default;
            kernelPackages = atlas-kernel.legacyPackages.${system}.linuxPackages-cachyos-latest-lto;
            name = "amc-shared-host-admission-atlas-kernel";
          };
        }
      );
    };
}
