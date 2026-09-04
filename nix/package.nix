{
  lib,
  rustPlatform,
}:

rustPlatform.buildRustPackage {
  pname = "amc";
  version = "0.1.0";

  src = lib.cleanSource ../.;
  cargoLock.lockFile = ../Cargo.lock;

  strictDeps = true;

  meta = {
    description = "Apply hard memory contracts to Linux applications";
    license = with lib.licenses; [
      asl20
      mit
    ];
    mainProgram = "amc";
    platforms = lib.platforms.linux;
  };
}
