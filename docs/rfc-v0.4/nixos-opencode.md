# Native OpenCode Integration

Status: declarative drop-in pattern demonstrated in the disposable
mechanism VM (package fragment + NixOS `asDropin` merge confirmed
effective; see `proof-results.md`). Home Manager evaluation of an actual
owner unit remains unproven. No production values selected, files installed,
services reloaded or restarted.

## Discovered Owner

Historical observations from the earlier session on 2026-09-06 identified:

- `opencode.service`: shared backend, `Type=simple`, `Restart=always`,
  `OOMPolicy=kill`, `KillMode=control-group`, `Slice=dev-agents.slice`, with
  dependencies on the existing network/OmniRoute arrangement.
- `opencode --auto` frontend processes in separate scopes. Tool commands in
  that session were descendants of the backend. This does not prove every
  plain `opencode` invocation, desktop action or native activation variant.
- The observed ancestor had high/max/swap settings of 24/32/8 GiB. The backend
  itself was unlimited. These are historical observations, not selected defaults.

Original kernel byte counts, correctly converted using 2^30 bytes/GiB:

| Observation | Current | Peak |
|---|---:|---:|
| Earlier read (exact timestamp not retained) | 7,624,642,560 B = 7.101 GiB | 15,359,488,000 B = 14.305 GiB |
| Later inspect (exact timestamp not retained) | 12,751,990,784 B = 11.876 GiB | 18,324,422,656 B = 17.066 GiB |

The earlier prose incorrectly used decimal-GB magnitudes with GiB labels.
Neither observation measures a minimum required working set, nor proves that
applying a particular lower limit would necessarily kill the backend: reclaim
and workload behavior matter. They do not justify the old 6 GiB example as
production policy. No fresh live measurement was made during this corrective pass.

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

No active policy is set by this template. Do not import it assuming it contains
chosen limits. Preserve `ExecStart`, environment, dependencies, service type,
delegation, pressure handling, OOM/shutdown/restart policy and slice assignment.

Why `systemd.user.units` here: evaluating the pinned higher-level
`systemd.user.services.*.serviceConfig` mechanism generated additional
`Environment` entries, including PATH. Evaluating the lower-level native `text`
drop-in generated only the selected fragment. This avoids changing the existing
owner's execution environment. The disposable VM exercises package-provided
units and a higher-priority per-user main-unit symlink with this mechanism.

The repository has no pinned Home Manager input. The symlink test exercises
systemd lookup behavior, not Home Manager module evaluation. A change directly
inside the actual Home Manager owner must be built and inspected in that
configuration's existing pinned environment before claiming compatibility.

## Declarative Procedure

These are **manual maintenance instructions**, not actions performed by AMC.
Use your configuration's approved deployment workflow and its actual flake
reference. Do not change the main unit's owner to make these commands convenient.

1. Add only the reviewed resource fragment to the existing NixOS configuration.
2. Build the configuration without activation (`nixos-rebuild build --flake
   CONFIGURATION#HOST`, or its approved repository equivalent). Inspect the
   generated drop-in and existing main-unit sources in the built output.
   `systemctl cat` before installation shows the old installed configuration,
   not the newly built one.
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

Declarative rollback: remove this fragment, build and review again, install the
prior configuration through the same approved workflow, and activate during a
maintenance window. Editing a Nix source alone, followed by `daemon-reload`,
does not install or roll back declarative configuration. Verify the restored
effective values against the saved pre-change observation, not an assumed
unlimited value.

## Manual File Procedure

If deliberately choosing a manual file instead of declarative ownership, use
only `~/.config/systemd/user/opencode.service.d/90-amc-memory.conf`:

```ini
[Service]
# Add reviewed MemoryHigh/MemoryMax/MemorySwapMax values here.
```

Save the prior contents if that exact file already exists. Review the complete
installed unit and drop-ins. During a maintenance window, reload configuration
with `systemctl --user daemon-reload`, then restart only `opencode.service` if
needed to apply the policy to a fresh execution. This interrupts its users.
Verify effective settings rather than treating file content as the result.

Manual rollback: restore/remove only that file, reload the user manager, and
restart the backend only during another suitable maintenance window. Do not
delete other drop-ins or reset the service's existing lifecycle configuration.

## Coverage And Trial

The selected policy covers the existing backend and ordinary descendants.
Frontend scopes, Nix/Lix builders, containers and remote execution remain
separate domains. No whole-daemon limits or group-killing changes are proposed.

An isolated OpenCode trial remains a **separate decision, not yet taken**:
the mechanism VM has now executed (see `proof-results.md`), but a trial still
requires a separately configured disposable session with no paid requests or
state-changing agent work to measure read-only latency and completed work.
Synthetic fixture success is not an OpenCode freeze fix.
