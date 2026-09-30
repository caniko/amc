# Gaming-first workstation use

The objective is a stable, usable foreground gaming session while selected
background jobs remain bounded and make useful progress under shared resource
pressure. AMC supplies admission and diagnostics; systemd owns execution and
cgroups. The consumer selects budgets, priorities, enrolled jobs and deployment.
Fleetix integration is optional. The game keeps its ordinary launch path.

## Choose the resource domains

Use a native aggregate memory/swap ceiling for each enrolled background pool,
per-job ceilings, a concurrency bound, and a host safety reserve. Give background
parents **and** their children lower CPU/I/O weights than the foreground domain.
Lower leaf weights alone do not lower an equally weighted ancestor's share.
These are scheduling preferences, not fixed latency guarantees.

Admission checks live headroom before starting participating jobs. Existing jobs
keep their grants until confirmed termination. Other users, native application
backends, daemon builders, inference and remote workers need their own consumer
budgets and native boundaries. A capped `nix build` client does not cap builders
owned by the Nix daemon. Budget shares and hard ceilings are not preallocated RAM.

Useful work can continue at lower priority throughout gaming. Optional maintenance
may be deferred separately. Setting `pause_file` blocks **new** admissions for
that contract while the marker exists; it neither frees memory nor stops existing
jobs. Do not pause every pool when continued useful progress is the selected goal.

## Standalone systemd setup

Install the CLI using the [README](../README.md#install-and-first-report). Linux,
systemd user services and delegated cgroup v2 memory controls are required for
managed execution. The embeddable gates and offline reports remain independent.

The checkout supplies [three example units](../examples/systemd/) and a
[JSON policy](../examples/admission.json). Nix installations also provide these
under `share/amc/examples` in the package output. This example has a 2 GiB native
aggregate ceiling, no swap, a 1 GiB shared admission budget, two concurrent
512 MiB jobs and a 2 GiB host reserve. **Choose values for your machine and work**;
the example is a small starting configuration, not a universal workstation policy.

The following commands explicitly use Bash, from the checkout:

```bash
install -d -m 700 "$HOME/.config/amc" "$HOME/.config/systemd/user"
install -m 600 examples/admission.json "$HOME/.config/amc/admission.json"
install -m 600 examples/systemd/amc.slice "$HOME/.config/systemd/user/amc.slice"
install -m 600 examples/systemd/amc-background.slice "$HOME/.config/systemd/user/amc-background.slice"
install -m 600 examples/systemd/amc-admission.service "$HOME/.config/systemd/user/amc-admission.service"
command -v amc
```

The service's `ExecStart` defaults to `%h/.cargo/bin/amc`, the normal Cargo
installation. For another installation, edit that line in the copied service to
the absolute executable path printed above. Preserve the arguments and policy
path. On distributions with systemd outside `/usr/bin`, include its directory in
the service's `Environment=PATH`; the Nix AMC wrapper supplies its own tools.

```bash
systemctl --user daemon-reload
systemctl --user enable --now amc-admission.service
amc admission status --json
systemctl --user show amc.slice amc-background.slice \
  --property=LoadState,ActiveState,ControlGroup,MemoryMax,MemorySwapMax,CPUWeight,IOWeight
```

Confirm both slices are active with the intended effective limits. `amc inspect`
additionally reads kernel limits for the actual job service. An unavailable
controller or unknown native headroom refuses admission rather than bypassing it.

## Enroll a finite useful job

For example, archive a selected source directory into a **new** output file:

```bash
amc admission exec --contract background --timeout 120 --runtime-max-sec 300 -- \
  tar -cf /absolute/new/source.tar -C /absolute/source .
tar -tf /absolute/new/source.tar
```

Replace the paths and verify the output of your actual job. Use distinct output
paths so a previous artifact cannot be mistaken for completed work.

`--timeout` bounds admission waiting (1–3600 seconds). `--runtime-max-sec`
optionally requests systemd's execution deadline (1–86400 seconds); its stop
timeout also applies, so it is not an exact wall-clock completion promise.
The native deadline survives submitting-client and coordinator death. It is
omitted by default for explicitly enrolled long-lived services. SIGINT/SIGTERM
requests cancellation of this client's owned job and its descendants.

While work is queued, `amc admission status` shows the reason: concurrency,
budget, shared/slice headroom, pause, FIFO order or unavailable native evidence.
Entered jobs remain charged until native termination is confirmed. A timeout,
client death or missing socket is not permission to delete the ledger or replay
an uncertain command. See [persistent admission](persistent-admission.md).

## Upgrade, disable and remove

Preserve the private ledger during upgrades. Replace the installed package or
policy, restart `amc-admission.service`, and inspect status. Entered work retains
its old contract until termination; queued/unentered requests are invalidated.
For Home Manager use the [module lifecycle](persistent-admission.md#installation-and-removal).

For a standalone installation, stop enrolling new work and let entered jobs
finish, or cancel their exact owned invocations. Require zero commitments and no
unreconciled entries before disabling the service:

```bash
amc admission status --json
systemctl --user disable --now amc-admission.service
```

To remove it, remove the three unit files copied above, run `daemon-reload`, and
stop the now-unused `amc-background.slice` and `amc.slice`. Remove the policy and
CLI through their original installation path. Preserve the ledger until all
recorded workloads are confirmed terminated. Disabling the coordinator refuses
new managed starts; it does not remove limits from existing native jobs.

## Operational acceptance

A usable profile has a short functional acceptance path:

1. Start the ordinary foreground session and one intended useful background job.
   Inspect its actual unit, ancestry and limits; verify its output.
2. Offer more work than the configured capacity. Check that excess work queues
   with a reason or times out clearly, and drains when capacity returns.
3. Check foreground exit and interrupted hooks restore the intended maintenance
   policy without leaving a stale freeze or pause marker.
4. Check cancellation, a bounded-job failure, coordinator restart and subsequent
   work. Unknown identities stay accounted for until reconciled.
5. Check installation, upgrade, disable and removal for the shipped module.

The explicit native fixtures `tests/admission-systemd.py` and
`tests/admission-install-systemd.py` exercise these lifecycle mechanisms with
small isolated jobs. The latter installs uniquely named copies of the shipped
units and preserves its logs and ledger in a new `--output` directory.
The two-user admission VM invokes both fixtures.

Comparative frame-time results are optional diagnostics. The
[CS2 guide](gaming-comparison.md) retains recording and measurement instructions
for users who want them; they are not prerequisites for this operational profile.
