# Thin wrapper around `amc admission exec` for consumers that route managed
# services through a stable executable path.
{
  callPackage,
  lib,
  writeShellApplication,
  amc ? callPackage ./package.nix { },
  name ? "amc-service-exec",
}:
writeShellApplication {
  inherit name;
  text = ''
    # The Rust client owns queueing, native entry verification, and cancellation.
    exec ${lib.getExe amc} admission exec "$@"
  '';
}
