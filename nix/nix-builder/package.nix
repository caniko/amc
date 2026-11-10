# Nix patched to own aggregate builder cgroups and consult host admission.
# Pair with `lib.nixBuilderSettings`; this neither installs the package nor
# chooses budgets.
{pkgs}: let
  inherit (pkgs) lib;
  headerLines = lib.splitString "\n" (lib.removeSuffix "\n" (builtins.readFile ./nix-build-policy.hh));
  # Nixpkgs packages Nix as separate libraries and a CLI. Patch the shared
  # source so every consumer of the package set gets identical build ownership.
  headerPatch = pkgs.writeText "nix-build-policy.patch" (''
      diff --git a/src/libutil/linux/include/nix/util/nix-build-policy.hh b/src/libutil/linux/include/nix/util/nix-build-policy.hh
      --- /dev/null
      +++ b/src/libutil/linux/include/nix/util/nix-build-policy.hh
      @@ -0,0 +1,${toString (builtins.length headerLines)} @@
    ''
    + lib.concatMapStrings (line: "+${line}\n") headerLines);
  hostLines = lib.splitString "\n" (lib.removeSuffix "\n" (builtins.readFile ./nix-host-admission.hh));
  hostHeaderPatch = pkgs.writeText "nix-host-admission-header.patch" (''
      diff --git a/src/libutil/linux/include/nix/util/nix-host-admission.hh b/src/libutil/linux/include/nix/util/nix-host-admission.hh
      --- /dev/null
      +++ b/src/libutil/linux/include/nix/util/nix-host-admission.hh
      @@ -0,0 +1,${toString (builtins.length hostLines)} @@
    ''
    + lib.concatMapStrings (line: "+${line}\n") hostLines);
in
  # The patches target the 2.34 source API. Point releases flow through; one
  # that conflicts fails patchPhase. A new minor series needs a rebase.
  assert lib.assertMsg (lib.versions.majorMinor pkgs.nix.version == "2.34") "Rebase the AMC Nix builder patches before changing the Nix minor series.";
    pkgs.nix.appendPatches [headerPatch hostHeaderPatch ./nix-builder-cgroups.patch ./nix-builder-admission.patch ./nix-host-admission.patch ./nix-drain-continuations.patch ./nix-worker-operations.patch ./nix-overflow-capacity.patch]
