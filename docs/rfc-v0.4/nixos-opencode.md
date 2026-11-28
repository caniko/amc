# Native OpenCode Integration

The disposable mechanism VM confirmed the declarative drop-in pattern: the
package fragment and NixOS `asDropin` merged as intended (see `proof-results.md`).
Home Manager evaluation of an actual owner unit remains unproven. No production
values selected, files installed, or services reloaded or restarted.

## Discovered Owner

The 2026-09-06 session showed:

- `opencode.service`: shared backend, `Type=simple`, `Restart=always`,
  `OOMPolicy=kill`, `KillMode=control-group`, `Slice=dev-agents.slice`, with
  existing network/OmniRoute dependencies.
- `opencode --auto` frontends in separate scopes; tool commands descended from
  the backend. This does not establish coverage for every plain `opencode`
  invocation, desktop action, or native activation variant.
- Ancestor high/max/swap settings of 24/32/8 GiB and an unlimited backend.
  These are observations, not defaults.

Original kernel counts, converted at 2^30 bytes/GiB:

| Observation | Current | Peak |
|---|---:|---:|
| Earlier read (exact timestamp not retained) | 7,624,642,560 B = 7.101 GiB | 15,359,488,000 B = 14.305 GiB |
| Later inspect (exact timestamp not retained) | 12,751,990,784 B = 11.876 GiB | 18,324,422,656 B = 17.066 GiB |

Earlier prose mislabeled decimal-GB values as GiB. Neither reading establishes
a minimum working set, proves a lower limit would kill the backend (reclaim
and workload behavior matter), or justifies the old 6 GiB example in production.
No fresh live measurements were taken for this correction.

## Declarative Fragment

Keep the existing Home Manager-owned main unit. Use a targeted native NixOS
drop-in, not a new service or wrapper:

```nix
{
  systemd.user.units."opencode.service" = {
    overrideStrategy = "asDropin";
    text = ''
      [Service]
      # Add reviewed MemoryHigh/MemoryMax/MemorySwapMax values here.
    '';
  };
}
```

This template sets no active policy; do not import it expecting chosen limits.
Preserve `ExecStart`, environment, dependencies, service type,
delegation, pressure handling, OOM/shutdown/restart policy and slice assignment.

Use `systemd.user.units` here: evaluation of the pinned higher-level
`systemd.user.services.*.serviceConfig` generated extra `Environment` entries,
including PATH. The lower-level native `text` drop-in generated only the
selected fragment, leaving the owner's execution environment alone. The
disposable VM tests this with package-provided units and a higher-priority
per-user main-unit symlink.

The repository has no pinned Home Manager input. The symlink tests systemd
lookup, not Home Manager module evaluation. Build and inspect changes to the
actual Home Manager owner in its existing pinned environment before claiming
compatibility.

## Declarative Procedure

These are **manual maintenance instructions**; AMC does not perform them.
Use your configuration's approved deployment workflow and actual flake reference.
Do not change the main unit's owner for convenience.

1. Add only the reviewed resource fragment to the existing NixOS configuration.
2. Build the configuration without activation (`nixos-rebuild build --flake
   CONFIGURATION#HOST`, or its approved repository equivalent). Inspect the
   generated drop-in and existing main-unit sources in the built output.
   Before installation, `systemctl cat` shows the installed configuration,
   not the new build.
3. Compare executable, environment, lifecycle and dependency settings before
   installing. Check all relevant user/system drop-in paths: higher-priority
   or later-named drop-ins may override the fragment.
4. Schedule activation after closing sessions that depend on the backend.
   Installing with `nixos-rebuild boot --flake CONFIGURATION#HOST` defers
   activation to the next planned boot. A switch/Home Manager activation can
   reload or restart services automatically; never run it from this agent session.
5. After that scheduled activation, verify manager properties, fragment/drop-in
   provenance and leaf/ancestor kernel observations with `amc inspect`.
   Continue the existing observed frontend launch path; do not add `amc run`.

Declarative rollback: remove the fragment, rebuild and review, install the
prior configuration through the same approved workflow, and activate in a
maintenance window. A Nix source edit and `daemon-reload` alone do not install
or roll back declarative configuration. Verify restored effective values against
the saved pre-change observation; do not assume they were unlimited.

## Manual File Procedure

For a deliberate manual override instead of declarative ownership, use
only `~/.config/systemd/user/opencode.service.d/90-amc-memory.conf`:

```ini
[Service]
# Add reviewed MemoryHigh/MemoryMax/MemorySwapMax values here.
```

Save any existing contents of that exact file. Review the complete installed
unit and drop-ins. In a maintenance window, run `systemctl --user daemon-reload`,
then restart only `opencode.service` if a fresh execution is needed to apply
the policy. Restarting interrupts users. Verify effective settings, not just
file contents.

Manual rollback: restore/remove only that file, reload the user manager, and
restart the backend only during another suitable maintenance window. Do not
delete other drop-ins or reset the service's existing lifecycle configuration.

## Coverage And Trial

The selected policy covers the existing backend and ordinary descendants.
Frontend scopes, Nix/Lix builders, containers and remote execution remain
separate domains. No whole-daemon limits or group-killing changes are proposed.

The mechanism VM ran (see `proof-results.md`); an isolated OpenCode trial is a
**separate decision, not yet taken**. Any trial still requires a separately
configured disposable session with no paid requests or state-changing agent work;
use it to measure read-only latency and completed work.
Synthetic fixture success is not an OpenCode freeze fix.
