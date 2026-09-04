{
  description = "Application Memory Contracts";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "aarch64-linux"
        "x86_64-linux"
      ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
      pkgsFor = system: import nixpkgs { inherit system; };
    in
    {
      packages = forAllSystems (system: {
        default = (pkgsFor system).callPackage ./nix/package.nix { };
      });

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
            });
        in
        {
          fmt = mkCargoCheck "fmt" [ pkgs.rustfmt ] "cargo fmt --all --check";
          clippy = mkCargoCheck "clippy" [ pkgs.clippy ] "cargo clippy --all-targets --locked -- -D warnings";
          test = mkCargoCheck "test" [ ] "cargo test --all-targets --locked";
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

      # Deliberately excluded from checks: run explicitly with
      # nix build .#nixosTests.x86_64-linux.generic
      nixosTests = forAllSystems (
        system:
        let
          pkgs = pkgsFor system;
        in
        {
          generic = import ./nix/vm-test.nix {
            inherit pkgs;
            amcModule = self.nixosModules.default;
            amcPackage = self.packages.${system}.default;
          };
        }
      );
    };
}
