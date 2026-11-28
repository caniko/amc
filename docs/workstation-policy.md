# Gaming-first workstation use

Keep foreground gaming stable and usable under shared resource pressure while
selected background jobs stay within bounds and make useful progress. AMC
handles admission and diagnostics; systemd owns execution and cgroups. You
choose budgets, priorities, enrolled jobs and deployment. Fleetix integration
is optional. Launch the game as usual.

## Choose the resource domains

Set a native aggregate memory/swap ceiling for each enrolled background pool,
per-job ceilings, a concurrency bound and a host safety reserve. Assign lower
CPU/I/O weights than the foreground domain to background parents **and**
children. Lower leaf weights alone won't reduce an equally weighted ancestor's
share. Weights guide scheduling; they don't guarantee fixed latency.

Admission checks live headroom before starting participating jobs and holds
existing grants until it confirms termination. Set separate budgets and native
boundaries for other users, native application backends, daemon builders,
inference and remote workers. Capping a `nix build` client doesn't cap the Nix
daemon's builders. Budget shares and hard ceilings don't preallocate RAM.

Useful work can run at lower priority throughout gaming. Defer optional
maintenance separately if needed. Setting `pause_file` blocks **new** admissions
for that contract while the marker exists; it neither frees memory nor stops
existing jobs. To keep useful work going, don't pause every pool.

## Standalone systemd setup

Install the CLI using the [README](../README.md#install-and-first-report).
Managed execution requires Linux, systemd user services and delegated cgroup v2
memory controls. Embeddable gates and offline reports don't depend on this setup.

Find [three example units](../examples/systemd/) and a
[JSON policy](../examples/admission.json) in the checkout or under
`share/amc/examples` in the Nix package output. The example sets a 2 GiB native
aggregate ceiling, no swap, a 1 GiB shared admission budget, two concurrent
512 MiB jobs and a 2 GiB host reserve. **Choose values for your machine and work**;
this small example isn't a universal workstation policy.

Run these commands in Bash from the checkout:

```bash
install -d -m 700 "$HOME/.config/amc" "$HOME/.config/systemd/user"
install -m 600 examples/admission.json "$HOME/.config/amc/admission.json"
install -m 600 examples/systemd/amc.slice "$HOME/.config/systemd/user/amc.slice"
install -m 600 examples/systemd/amc-background.slice "$HOME/.config/systemd/user/amc-background.slice"
install -m 600 examples/systemd/amc-admission.service "$HOME/.config/systemd/user/amc-admission.service"
command -v amc
```

The service's `ExecStart` defaults to `%h/.cargo/bin/amc`, the normal Cargo
installation. For other installations, set the executable in the copied service
to the absolute path printed above. Keep the arguments and policy path. If your
distribution puts systemd outside `/usr/bin`, add its directory to the service's
`Environment=PATH`; the Nix AMC wrapper supplies its own tools.

```bash
systemctl --user daemon-reload
systemctl --user enable --now amc-admission.service
amc admission status --json
systemctl --user show amc.slice amc-background.slice \
  --property=LoadState,ActiveState,ControlGroup,MemoryMax,MemorySwapMax,CPUWeight,IOWeight
```

Confirm both slices are active with the intended effective limits. `amc inspect`
also reads kernel limits for the actual job service. Managed admission fails
closed and refuses entry if a controller is unavailable or native headroom is
unknown.

## Enroll a finite useful job

For example, archive a source directory to a **new** output file:

```bash
amc admission exec --contract background --timeout 120 --runtime-max-sec 300 -- \
  tar -cf /absolute/new/source.tar -C /absolute/source .
tar -tf /absolute/new/source.tar
```

Replace the paths and verify your job's output. Use distinct output paths so old
artifacts can't pass for completed work.

`--timeout` bounds admission waiting (1–3600 seconds). `--runtime-max-sec` can
request a systemd execution deadline (1–86400 seconds). Systemd's stop timeout
still applies, so the deadline doesn't promise exact wall-clock completion.
The native deadline survives the submitting client and coordinator dying. AMC
omits the request by default for explicitly enrolled long-lived services.
SIGINT/SIGTERM requests cancellation of this client's owned job and its
descendants.

`amc admission status` explains queued work: concurrency,
budget, shared/slice headroom, pause, FIFO order or unavailable native evidence.
Entered jobs stay charged until confirmed native termination. A timeout, client
death or missing socket does not by itself authorize deleting the ledger or
replaying an uncertain command. See [persistent admission](persistent-admission.md).

## Upgrade, disable and remove

Preserve the private ledger during upgrades. Replace the installed package or
policy, restart `amc-admission.service`, and inspect status. Entered work retains
its old contract until termination; queued/unentered requests are invalidated.
For Home Manager use the [module lifecycle](persistent-admission.md#installation-and-removal).

For standalone installs, stop enrolling new work. Let entered jobs finish or
cancel their exact owned invocations. Require zero commitments and no
unreconciled entries before disabling the service:

```bash
amc admission status --json
systemctl --user disable --now amc-admission.service
```

To uninstall, remove the three unit files copied above, run `daemon-reload`,
then stop the unused `amc-background.slice` and `amc.slice`. Remove the policy
and CLI through their original installation path. Keep the ledger until you
confirm all recorded workloads have terminated. Disabling the coordinator blocks
new managed starts without lifting limits on existing native jobs.

## Operational acceptance

1. Start the ordinary foreground session and one intended useful background job.
   Inspect its actual unit, ancestry and limits; verify its output.
2. Submit more work than configured capacity allows. Check that excess work
   queues with a reason or times out clearly, and that the queue drains when
   capacity returns.
3. Check foreground exit and interrupted hooks restore the intended maintenance
   policy without leaving a stale freeze or pause marker.
4. Check cancellation, a bounded-job failure, coordinator restart and subsequent
   work. Unknown identities stay accounted for until reconciled.
5. Check installation, upgrade, disable and removal for the shipped module.

The two-user admission VM runs both explicit native fixtures:
`tests/admission-systemd.py` and `tests/admission-install-systemd.py`. They test
these lifecycle mechanisms with small isolated jobs. The latter installs
uniquely named copies of the shipped units and keeps its logs and ledger in a new
`--output` directory. Its policy-upgrade check retains the old entered
reservation across restart and verifies that subsequent useful work receives
the new kernel memory ceiling.

Comparative frame-time results are optional diagnostics, not prerequisites for
this profile. The [CS2 guide](gaming-comparison.md) explains how to record and
measure them.
