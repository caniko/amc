# Application Memory Contracts

AMC v0.1 is a local NixOS proof that starts an application in a transient user
service with hard memory and swap limits already applied. A process that exceeds
the contract is killed as a cgroup without requiring a resident AMC daemon.

The narrow claim is: a directly executed Linux application can be bounded while
an unrelated heartbeat remains responsive. Work brokered by another daemon is
outside that claim.

```text
terminal -> amc -> systemd-run --user -> app-amc-....service -> application
                                           |
                         MemoryMax + MemorySwapMax + group OOM
```

## NixOS installation

```nix
{
  inputs.amc.url = "github:caniko/amc";
  outputs = { nixpkgs, amc, ... }: {
    nixosConfigurations.my-host = nixpkgs.lib.nixosSystem {
      modules = [
        amc.nixosModules.default
        {
          programs.amc = {
            enable = true;
            settings = {
              version = 1;
              profiles.interactive = {
                slice = "app-amc.slice";
                # Measure your own workload and RAM before choosing these.
                memory_max = "6GiB";
                memory_swap_max = "512MiB";
              };
              applications."ai.opencode".profile = "interactive";
            };
          };
        }
      ];
    };
  };
}
```

The module installs `amc`, writes `/etc/xdg/amc/config.toml`, and creates
`app-amc.slice` and `background-amc.slice` beneath the standard user slices.
It installs no service or timer.

## Commands

```sh
amc doctor
amc explain --id ai.opencode
amc run --id ai.opencode -- command arg
amc launch --id ai.opencode -- graphical-command
amc inspect app-amc-ai-opencode-01234567@89abcdef.service
```

`run` waits and uses a PTY only when all three standard streams are terminals;
otherwise it uses inherited pipes. `launch` returns after systemd verifies
`execve` startup and sends output to the journal. Add `--retain-unit` when
post-mortem inspection is needed, then clean up with:

```sh
systemctl --user stop UNIT
systemctl --user reset-failed UNIT
```

Configuration search order is explicit `--config`,
`$XDG_CONFIG_HOME/amc/config.toml`, `$HOME/.config/amc/config.toml`, then
`/etc/xdg/amc/config.toml`. Files are not merged. Limits are integral `B`,
`KiB`, `MiB`, `GiB`, or `TiB` values.

## Proofs

The real-host proof is bounded to a 256 MiB cgroup and cleans up through a trap:

```sh
nix develop -c ./scripts/prove-local.sh
```

Fast checks do not build a VM:

```sh
nix flake check
```

Run the heavyweight VM proof explicitly:

```sh
nix build .#nixosTests.x86_64-linux.generic
```

## OpenCode trial

`examples/config.toml` uses a prepared 6 GiB memory / 512 MiB swap contract,
based on one Atlas observation with headroom. Re-measure before production use.
After the synthetic proof passes, run manually:

```sh
amc --config examples/config.toml explain --id ai.opencode
amc --config examples/config.toml run --id ai.opencode -- opencode
```

Monitor the reported unit with `amc inspect UNIT`, `systemd-cgtop`, and
`/proc/pressure/memory`. Test OpenCode without a Nix build first.

## Nix boundary

Multi-user Nix builders run under `nix-daemon.service`, not the calling AMC user
cgroup. `amc run -- nix build ...` limits only the client. The VM includes a
synthetic derivation that demonstrates the escape location.

`programs.amc.nixBuildPool` is disabled and rejects enablement. An aggregate
daemon cap has host-wide blast radius and requires a separate VM test proving
OOM behavior, socket recovery, and a subsequent successful build before it can
be offered.

## Rollback

Remove the AMC module and rebuild NixOS. Stop/reset any retained transient unit;
there is no resident daemon or persistent state to remove. User slice units
disappear with the module on the next activation/login.

## Non-goals

v0.1 has no automatic sizing, percentages, learning, dynamic resize,
`MemoryHigh`, systemd-oomd policy, restart, CPU/I/O controls, desktop metadata,
portal, direct D-Bus client, or cross-daemon contract propagation. The next
phase is the isolated Nix build-pool recovery experiment described in
`docs/known-boundaries.md`.
