{
  lib,
  makeWrapper,
  python3,
  rustPlatform,
  systemd,
}:
rustPlatform.buildRustPackage {
  pname = "amc";
  version = "0.1.0";

  # Only Cargo build inputs: most docs/, scripts/, and nix/ edits do not
  # rebuild the package. The two embedded exceptions are nix/vm-test.py
  # (systemd fixture) and scripts/validate-capture.py (offline report).
  src = lib.cleanSourceWith {
    src = ../.;
    filter = path: _type: let
      rel = lib.removePrefix (toString ../. + "/") (toString path);
    in
      builtins.elem (builtins.head (lib.splitString "/" rel)) [
        "Cargo.toml"
        "Cargo.lock"
        "src"
        "crates"
        "examples"
        "tests"
      ]
      || rel == "nix"
      || rel == "nix/vm-test.py"
      || rel == "scripts"
      || rel == "scripts/validate-capture.py";
  };
  cargoLock.lockFile = ../Cargo.lock;
  cargoBuildFlags = ["-p" "amc"];
  cargoTestFlags = ["-p" "amc"];

  strictDeps = true;
  nativeBuildInputs = [makeWrapper python3];
  postInstall = ''
    mkdir -p $out/share/amc/examples
    cp -r examples/systemd $out/share/amc/examples/
    cp examples/admission.json $out/share/amc/examples/
  '';
  postFixup = ''
    wrapProgram "$out/bin/amc" --prefix PATH : ${lib.makeBinPath [python3 systemd]}
  '';

  meta = {
    description = "Linux memory admission, passive diagnostics, and native policy fixtures";
    license = with lib.licenses; [
      asl20
      mit
    ];
    mainProgram = "amc";
    platforms = lib.platforms.linux;
  };
}
