# Patient native execution

`amc admission exec --queue --max-ram-usage 2GiB --runtime-max-sec 30 -- command`
opts into resource waiting without a default expiry. `--timeout` bounds resource
waiting explicitly; legacy invocations retain their 120-second default. Policy
caps, including the protected-pool wait limit, still apply. Both the flake library
and installed package advertise `admissionQueueVersion = 1`.

The private coordinator first reserves the declared byte ceiling. A finite
30-second native startup window then pins the helper's invocation, cgroup inode,
PID and enforcement. `starting` retains that reservation while the host broker
queues the same native identity. This does not permit payload execution. A host
grant followed by the authenticated private `Enter` transition is still required.

Queued services use `Type=notify`, `NotifyAccess=main` and `TimeoutStartSec=30s`.
While waiting, the main helper extends startup by 30 seconds every five seconds.
It sends `READY=1` only immediately before executing the admitted command; native
`RuntimeMaxSec` therefore does not include host queue time. Notifications stop
before execution, and the payload does not inherit `NOTIFY_SOCKET`. These are
the native [systemd service startup and runtime semantics](https://www.freedesktop.org/software/systemd/man/latest/systemd.service.html).

Callers with their own execution clocks can pass `--ready-token TOKEN`. After
authenticated private entry and the final cancellation check, the native helper
writes `RS TOKEN US` (bytes `0x1e`, token, `0x1f`) to stderr immediately before
exec. Tokens contain 32–64 ASCII letters, digits or hyphens. A caller must generate
a fresh unpredictable token per invocation, strip just that frame from captured
output, and keep waiting/queued until it arrives. A shim that exits first has not
confirmed execution. This is a start notification, not an execution acknowledgement
or replay guarantee. The library/package advertise `admissionReadyVersion = 1`;
older packages must not be opted into this protocol.

The 30-second startup grace, resource wait, two-second broker exchange and native
execution deadline remain separate. Explicit resource deadlines are checked
again after native verification. Cancellation, expiry and lost acknowledgements
never release a starting or running native reservation; only positive cleanup
does. A private coordinator restart retains native accounting and stops starting
helpers because their volatile execution capabilities no longer exist. It never
replays the command. Legacy clients and finite host requests remain compatible;
older brokers reject omitted host wait fields and the private `Start` operation.

## Qualification

- `crates/amc-admission/tests/queue_clocks.rs` checks accounting through waits
  beyond 120 seconds, finite expiry, restart, identity changes and cleanup.
- `nix/queued-admission-vm-test.py` runs in both shared-host VM gates. It requires
  a 125-second host wait followed by successful execution within a two-second
  runtime, control-status p95 at most one second under saturation, and no payload
  execution after finite expiry, cancellation or private coordinator restart.
- Existing private-entry cancellation and descendant-cleanup gates stay enabled.

The VM acceptance is pending until the exact producer revision passes those
hosted gates. This capability alone does not retire OpenCode lifetime slots,
persist executable OpenCode payloads, or establish preparation/nested-work
measurements. Blocking Unix-socket connection setup also remains outside the
50ms cancellable framing bound. Consumer clock separation must be verified at
its own shell boundary, not inferred from the native service result.
