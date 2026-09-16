{
  lib,
  rustPlatform,
}:
rustPlatform.buildRustPackage {
  pname = "amc";
  version = "0.1.0";

  # Only Cargo build inputs: harness-only edits (docs/, scripts/, most of
  # nix/) must not rebuild the package or invalidate its cache entry. The one
  # exception is nix/vm-test.py, embedded by src/systemd.rs via include_str!.
  src = lib.cleanSourceWith {
    src = ../.;
    filter = path: _type:
      let rel = lib.removePrefix (toString ../. + "/") (toString path); in
      builtins.elem (builtins.head (lib.splitString "/" rel)) [
        "Cargo.toml"
        "Cargo.lock"
        "src"
        "crates"
        "tests"
      ]
      || rel == "nix"
      || rel == "nix/vm-test.py";
  };
  cargoLock.lockFile = ../Cargo.lock;
  cargoBuildFlags = ["-p" "amc"];
  cargoTestFlags = ["-p" "amc"];

  strictDeps = true;

  meta = {
    description = "Disposable memory-policy fixtures and diagnostics for Linux user services (RFC 0.4 harness)";
    license = with lib.licenses; [
      asl20
      mit
    ];
    mainProgram = "amc";
    platforms = lib.platforms.linux;
  };
}
