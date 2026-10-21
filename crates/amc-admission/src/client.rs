//! In-process client for admitted native execution.
//!
//! [`Exec`] performs what `amc admission exec` does: enqueue with the per-user
//! coordinator, wait for a reservation, submit one transient systemd unit and
//! map its native outcome. The unit still runs the AMC `helper` executable for
//! the in-unit startup barrier and entry handshake, so callers pass the
//! approved `amc` binary path explicitly.
use crate::{
    ledger::{Backpressure, Contract, Phase, QueueRequest},
    protocol::{Message, call},
};
use amc_runner::systemd::{ClientRecord, Outcome, StartupBarrier, execute};
use anyhow::{Context, Result, ensure};
use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    fs::File,
    io::{PipeReader, PipeWriter, Read, Write},
    os::{
        fd::{AsFd, OwnedFd},
        unix::process::ExitStatusExt,
    },
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Output, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

/// Default wait for non-queued admission, matching `amc admission exec`.
const DEFAULT_WAIT: Duration = Duration::from_secs(120);

/// The per-user coordinator socket, `$XDG_RUNTIME_DIR/amc/admission.sock`.
pub fn default_socket() -> Result<PathBuf> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .ok_or_else(|| anyhow::anyhow!("XDG_RUNTIME_DIR is required for admission"))?;
    Ok(PathBuf::from(runtime).join("amc/admission.sock"))
}

/// How the requested work is sized against its contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    /// Patient queued work with an explicit memory ceiling and execution deadline.
    Queued {
        memory_max: u64,
        runtime_max_sec: u64,
    },
    /// Explicit ceiling that may use protected backing.
    Bounded {
        memory_max: u64,
        runtime_max_sec: u64,
    },
    /// Explicit per-call ceiling bounded by the contract.
    Sized { memory_max: u64 },
    /// Contract defaults.
    Plain,
}

/// One admission request, equivalent to the `amc admission exec` flags.
#[derive(Debug, Clone)]
pub struct ExecRequest {
    pub contract: String,
    pub mode: Mode,
    /// Admission wait. `None` is unbounded for queued work and 120 s otherwise.
    pub wait: Option<Duration>,
    /// Optional native execution deadline, bounded by the contract.
    pub runtime_max_sec: Option<u64>,
    /// Request the policy-defined short-call burst lane.
    pub burst: bool,
    /// Emit native-entry, nonacceptance and completion frames on stderr.
    pub ready_token: Option<String>,
    pub socket: PathBuf,
    /// The approved `amc` executable run inside the unit.
    pub helper: PathBuf,
}

impl ExecRequest {
    /// Patient queued work on the default per-user socket.
    pub fn queued(
        contract: impl Into<String>,
        memory_max: u64,
        runtime_max_sec: u64,
        helper: impl Into<PathBuf>,
    ) -> Result<Self> {
        Ok(Self {
            contract: contract.into(),
            mode: Mode::Queued {
                memory_max,
                runtime_max_sec,
            },
            wait: None,
            runtime_max_sec: Some(runtime_max_sec),
            burst: false,
            ready_token: None,
            socket: default_socket()?,
            helper: helper.into(),
        })
    }

    fn effective_wait(&self) -> Option<Duration> {
        match self.mode {
            Mode::Queued { .. } => self.wait,
            _ => Some(self.wait.unwrap_or(DEFAULT_WAIT)),
        }
    }

    /// The coordinator message this request enqueues.
    pub fn message(&self) -> Message {
        let contract = self.contract.clone();
        let wait_ms = self.effective_wait().map(|wait| wait.as_millis() as u64);
        match self.mode {
            Mode::Queued {
                memory_max,
                runtime_max_sec,
            } => Message::EnqueueWaiting {
                contract,
                request: QueueRequest {
                    memory_max,
                    runtime_max_sec,
                    wait_ms,
                },
            },
            Mode::Bounded {
                memory_max,
                runtime_max_sec,
            } => Message::EnqueueBounded {
                contract,
                wait_ms: wait_ms.unwrap_or_default(),
                memory_max,
                runtime_max_sec,
            },
            Mode::Sized { memory_max } => Message::EnqueueSized {
                contract,
                wait_ms: wait_ms.unwrap_or_default(),
                memory_max,
            },
            Mode::Plain => Message::Enqueue {
                contract,
                wait_ms: wait_ms.unwrap_or_default(),
            },
        }
    }
}

/// A standard stream for [`Exec`]. Pipes created by the spawn are not offered:
/// the call supervises the unit until it ends, so nobody could service them.
/// Pass a file, a pipe end you drain yourself, or use [`Exec::output`].
#[derive(Debug)]
pub enum Io {
    Inherit,
    Null,
    Fd(OwnedFd),
}

impl Io {
    fn stdio(self) -> Stdio {
        match self {
            Io::Inherit => Stdio::inherit(),
            Io::Null => Stdio::null(),
            Io::Fd(fd) => Stdio::from(fd),
        }
    }
}

impl From<File> for Io {
    fn from(file: File) -> Self {
        Io::Fd(file.into())
    }
}

impl From<OwnedFd> for Io {
    fn from(fd: OwnedFd) -> Self {
        Io::Fd(fd)
    }
}

impl From<PipeReader> for Io {
    fn from(reader: PipeReader) -> Self {
        Io::Fd(reader.into())
    }
}

impl From<PipeWriter> for Io {
    fn from(writer: PipeWriter) -> Self {
        Io::Fd(writer.into())
    }
}

/// How an admitted workload ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// The workload (or nonacceptance, 75) exited with this code.
    Code(i32),
    /// The workload was cancelled by this signal.
    Signal(i32),
}

impl Exit {
    /// Shell-style code: `128 + signal` for cancellation.
    pub fn code(self) -> i32 {
        match self {
            Exit::Code(code) => code,
            Exit::Signal(signal) => 128 + signal,
        }
    }

    fn status(self) -> ExitStatus {
        match self {
            Exit::Code(code) => ExitStatus::from_raw((code & 0xff) << 8),
            Exit::Signal(signal) => ExitStatus::from_raw(signal & 0x7f),
        }
    }
}

/// An admitted command, configured like [`std::process::Command`].
#[derive(Debug)]
pub struct Exec {
    request: ExecRequest,
    argv: Vec<OsString>,
    current_dir: Option<PathBuf>,
    env: BTreeMap<OsString, Option<OsString>>,
    stdin: Option<Io>,
    stdout: Option<Io>,
    stderr: Option<Io>,
}

impl Exec {
    pub fn new<I, S>(request: ExecRequest, argv: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        Self {
            request,
            argv: argv
                .into_iter()
                .map(|arg| arg.as_ref().to_owned())
                .collect(),
            current_dir: None,
            env: BTreeMap::new(),
            stdin: None,
            stdout: None,
            stderr: None,
        }
    }

    pub fn request(&self) -> &ExecRequest {
        &self.request
    }

    pub fn argv(&self) -> &[OsString] {
        &self.argv
    }

    pub fn arg(&mut self, arg: impl AsRef<OsStr>) -> &mut Self {
        self.argv.push(arg.as_ref().to_owned());
        self
    }

    pub fn current_dir(&mut self, dir: impl AsRef<Path>) -> &mut Self {
        self.current_dir = Some(dir.as_ref().to_owned());
        self
    }

    pub fn env(&mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> &mut Self {
        self.env
            .insert(key.as_ref().to_owned(), Some(value.as_ref().to_owned()));
        self
    }

    pub fn env_remove(&mut self, key: impl AsRef<OsStr>) -> &mut Self {
        self.env.insert(key.as_ref().to_owned(), None);
        self
    }

    pub fn stdin(&mut self, io: impl Into<Io>) -> &mut Self {
        self.stdin = Some(io.into());
        self
    }

    pub fn stdout(&mut self, io: impl Into<Io>) -> &mut Self {
        self.stdout = Some(io.into());
        self
    }

    /// Ignored when a ready token relays stderr for entry framing.
    pub fn stderr(&mut self, io: impl Into<Io>) -> &mut Self {
        self.stderr = Some(io.into());
        self
    }

    /// Admit, run and supervise the workload; returns its exit code
    /// (`128 + signal` when cancelled, 75 for queued nonacceptance with a token).
    pub fn run(self, cancelled: impl Fn() -> Option<i32>) -> Result<i32> {
        run(self, cancelled).map(Exit::code)
    }

    /// Like [`Exec::run`], keeping whether the workload exited or was signalled.
    pub fn run_exit(self, cancelled: impl Fn() -> Option<i32>) -> Result<Exit> {
        run(self, cancelled)
    }

    /// Like [`Exec::run`], capturing stdout and, without a ready token, stderr.
    /// Collectors stop and are joined on every return path, including
    /// unresolved native outcomes.
    pub fn output(mut self, cancelled: impl Fn() -> Option<i32>) -> Result<Output> {
        let capture_stderr = self.request.ready_token.is_none();
        let stop = Arc::new(AtomicBool::new(false));
        let (stdout_reader, stdout_writer) = std::io::pipe()?;
        self.stdout = Some(stdout_writer.into());
        let stdout = collect(stdout_reader, stop.clone())?;
        let stderr = if capture_stderr {
            let (reader, writer) = std::io::pipe()?;
            self.stderr = Some(writer.into());
            Some(collect(reader, stop.clone())?)
        } else {
            None
        };
        // `run` drops every writer it holds before returning, so collectors
        // reach EOF once the unit's own descriptors close.
        let result = run(self, cancelled);
        if result.is_err() {
            stop.store(true, Ordering::SeqCst);
        }
        let join = |handle: thread::JoinHandle<std::io::Result<Vec<u8>>>| {
            handle
                .join()
                .map_err(|_| anyhow::anyhow!("output capture thread panicked"))?
                .context("capture admitted output")
        };
        let stdout = join(stdout);
        let stderr = stderr.map(join).transpose();
        let exit = result?;
        Ok(Output {
            status: exit.status(),
            stdout: stdout?,
            stderr: stderr?.unwrap_or_default(),
        })
    }
}

/// Read `reader` to EOF unless `stop` is raised first.
fn collect(
    mut reader: PipeReader,
    stop: Arc<AtomicBool>,
) -> Result<thread::JoinHandle<std::io::Result<Vec<u8>>>> {
    use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
    Ok(thread::Builder::new()
        .name("amc-output-capture".into())
        .spawn(move || {
            let (mut bytes, mut buffer) = (Vec::new(), [0; 8192]);
            while !stop.load(Ordering::SeqCst) {
                let mut fds = [PollFd::new(reader.as_fd(), PollFlags::POLLIN)];
                match poll(&mut fds, PollTimeout::from(100u8)) {
                    Ok(0) => continue,
                    Ok(_) => {}
                    Err(nix::errno::Errno::EINTR) => continue,
                    Err(error) => return Err(error.into()),
                }
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(n) => bytes.extend_from_slice(&buffer[..n]),
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(error) => return Err(error),
                }
            }
            Ok(bytes)
        })?)
}

fn run(exec: Exec, cancelled: impl Fn() -> Option<i32>) -> Result<Exit> {
    let Exec {
        request,
        argv,
        current_dir,
        env,
        stdin,
        stdout,
        stderr,
    } = exec;
    let socket = request.socket.as_path();
    let ready_token = request.ready_token.as_deref();
    let burst = request.burst;
    let deadline = request.effective_wait().map(|wait| Instant::now() + wait);
    let queued = matches!(request.mode, Mode::Queued { .. });
    let response = match call(socket, request.message()) {
        Ok(response) => response,
        Err(error) => {
            if queued
                && let Some(token) = ready_token
                && let Some(backpressure) = error.downcast_ref::<Backpressure>()
            {
                // Only explicit nonacceptance permits the intent owner to try
                // again. Exit, rather than retaining one process per overflow.
                std::io::stderr().lock().write_all(
                    format!(
                        "\x1e{token}:backpressure:{}\x1f",
                        backpressure.retry_after_ms
                    )
                    .as_bytes(),
                )?;
                return Ok(Exit::Code(75));
            }
            return Err(error);
        }
    };
    let host_socket = response.host_socket;
    if let Some(host_socket) = &host_socket
        && let Err(error) = verify_host_runner(host_socket)
    {
        if let Some(entry) = &response.entry {
            let _ = call(
                socket,
                Message::Cancel {
                    id: entry.id.clone(),
                },
            );
        }
        return Err(error);
    }
    let entry_key = response
        .entry_key
        .ok_or_else(|| anyhow::anyhow!("missing admission entry capability"))?;
    let mut entry = response
        .entry
        .ok_or_else(|| anyhow::anyhow!("missing admission ticket"))?;
    let runtime_max_sec = match execution_runtime(&entry.contract, burst, request.runtime_max_sec) {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = call(socket, Message::Cancel { id: entry.id });
            return Err(error);
        }
    };
    let short_call = burst || entry.contract.protected_pool.is_some();
    let queued_at = Instant::now();
    while entry.phase == Phase::Queued {
        if cancelled().is_some() || deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            let _ = call(socket, Message::Cancel { id: entry.id });
            anyhow::bail!("admission cancelled or timed out before submission");
        }
        thread::sleep(crate::host_server::poll_interval(queued_at));
        entry = call(
            socket,
            Message::Poll {
                id: entry.id.clone(),
            },
        )?
        .entry
        .ok_or_else(|| anyhow::anyhow!("missing admission ticket"))?;
    }
    ensure!(
        entry.phase == Phase::Reserved,
        "ticket has already been submitted"
    );
    let mut client = Command::new("systemd-run");
    client.args([
        "--user",
        "--quiet",
        "--wait",
        "--pipe",
        "--same-dir",
        "--expand-environment=no",
    ]);
    client.arg(if entry.queue.is_some() {
        "--service-type=notify"
    } else {
        "--service-type=exec"
    });
    if entry.queue.is_some() {
        client.arg("--property=NotifyAccess=main");
    }
    client.arg(format!("--unit={}", entry.unit()));
    for property in [
        format!("Slice={}", entry.contract.slice),
        format!("MemoryMax={}", entry.contract.memory_max),
        format!("MemorySwapMax={}", entry.contract.memory_swap_max),
        "MemoryAccounting=yes".into(),
        "OOMPolicy=kill".into(),
        "KillMode=control-group".into(),
        "Restart=no".into(),
        "TimeoutStartSec=30s".into(),
        format!("TimeoutStopSec={}s", if short_call { 1 } else { 15 }),
    ] {
        client.arg(format!("--property={property}"));
    }
    if let Some(seconds) = runtime_max_sec {
        client.arg(format!("--property=RuntimeMaxSec={seconds}s"));
        client.arg("--property=RuntimeRandomizedExtraSec=0");
    }
    if short_call {
        client.arg("--property=SendSIGKILL=yes");
        client.arg("--property=FinalKillSignal=SIGKILL");
    }
    if burst {
        eprintln!(
            "AMC burst: {} bytes reserved, {}s native runtime ceiling",
            entry.contract.memory_max,
            runtime_max_sec.unwrap_or_default()
        );
    }
    // systemd-run reads each forwarded value from its own environment.
    for (name, value) in &env {
        match value {
            Some(value) => client.env(name, value),
            None => client.env_remove(name),
        };
    }
    for name in forwarded_environment(&env) {
        client.arg(format!("--setenv={name}"));
    }
    // User services inherit the manager environment; removal must be explicit.
    for name in removed_environment(&env) {
        client.arg(format!("--property=UnsetEnvironment={name}"));
    }
    if let Some(dir) = &current_dir {
        client.current_dir(dir);
    }
    let record = wrap(&mut client, &request.helper)?;
    client
        .arg(&request.helper)
        .args(["admission", "enter", "--socket"])
        .arg(socket)
        .args(["--ticket", &entry.id, "--entry-key", &entry_key]);
    if entry.queue.is_some() {
        client.arg("--queue");
    }
    if let Some(token) = ready_token {
        client.arg("--ready-token").arg(token);
    }
    if let Some(host_socket) = host_socket {
        client.arg("--host-socket").arg(host_socket);
    }
    client.arg("--").args(&argv);
    if let Some(stdin) = stdin {
        client.stdin(stdin.stdio());
    }
    if let Some(stdout) = stdout {
        client.stdout(stdout.stdio());
    }
    // A reservation reply can arrive after the client's wait has expired.
    // The coordinator's extra entry grace must not authorize a late submission.
    if cancelled().is_some() || deadline.is_some_and(|deadline| Instant::now() >= deadline) {
        let _ = call(
            socket,
            Message::Cancel {
                id: entry.id.clone(),
            },
        );
        anyhow::bail!("admission cancelled or timed out before submission");
    }
    // A helper that fails before entry also completes natively. Relay its
    // stderr so only an observed entry frame authorizes completion proof.
    let entered = match ready_token {
        Some(token) => {
            let (reader, writer) = std::io::pipe()?;
            client.stderr(writer);
            Some(relay_entry(
                reader,
                format!("\x1e{token}\x1f").into_bytes(),
            )?)
        }
        None => {
            if let Some(stderr) = stderr {
                client.stderr(stderr.stdio());
            }
            None
        }
    };
    let outcome = execute(
        &mut client,
        Path::new("systemctl"),
        &entry.unit(),
        false,
        true,
        &cancelled,
        &record,
    );
    // Drop this process's writers so relays and captures see EOF.
    client
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    drop(client);
    // Cancel only this owned attempt. The server retains an entered workload's
    // reservation until native termination, including on ambiguous outcomes.
    let _ = call(
        socket,
        Message::Cancel {
            id: entry.id.clone(),
        },
    );
    // Completion confirmed native cleanup, so the relay reaches EOF promptly.
    if let Some(frame) = ready_token.and_then(|token| completion_frame(outcome, token))
        && entered.is_some_and(|seen| seen.recv_timeout(Duration::from_secs(1)) == Ok(true))
    {
        std::io::stderr()
            .write_all(frame.as_bytes())
            .context("notify caller of confirmed native completion")?;
    }
    outcome_exit(outcome, &entry.unit())
}

/// Environment names forwarded into the unit: the process environment with
/// `overrides` applied, minus systemd-owned and invalid names.
fn forwarded_environment(overrides: &BTreeMap<OsString, Option<OsString>>) -> Vec<String> {
    let mut names = std::env::vars_os()
        .map(|(name, _)| name)
        .filter(|name| !overrides.contains_key(name))
        .chain(
            overrides
                .iter()
                .filter(|(_, value)| value.is_some())
                .map(|(name, _)| name.clone()),
        )
        .filter_map(|name| name.into_string().ok())
        .filter(|name| {
            amc_runner::systemd::valid_environment_name(name)
                && !name.starts_with("SYSTEMD_")
                && !name.starts_with("LISTEN_")
                && !matches!(
                    name.as_str(),
                    "INVOCATION_ID"
                        | "MANAGERPID"
                        | "JOURNAL_STREAM"
                        | "NOTIFY_SOCKET"
                        | "WATCHDOG_PID"
                        | "WATCHDOG_USEC"
                )
        })
        .collect::<Vec<_>>();
    names.sort();
    names
}

/// Valid environment names the caller removed with [`Exec::env_remove`].
fn removed_environment(overrides: &BTreeMap<OsString, Option<OsString>>) -> Vec<String> {
    overrides
        .iter()
        .filter(|(_, value)| value.is_none())
        .filter_map(|(name, _)| name.to_str())
        .filter(|name| amc_runner::systemd::valid_environment_name(name))
        .map(str::to_owned)
        .collect()
}

/// Refuse entry from a namespace runner without a backed aggregate allowance.
pub fn verify_host_runner(socket: &Path) -> Result<()> {
    let placement = std::fs::read_to_string("/proc/self/cgroup")?;
    if placement.lines().any(|line| {
        line.rsplit('/').next().is_some_and(|leaf| {
            leaf.starts_with("app-amc-host-runner-") && leaf.ends_with(".service")
        })
    }) {
        ensure!(
            crate::host_server::call(
                socket,
                &crate::host_server::Request::NamespaceRunner { version: 1 }
            )?
            .granted,
            "namespace runner has no backed aggregate allowance"
        );
    }
    Ok(())
}

/// Insert the `helper native-start` startup barrier before the unit command.
/// The same outer helper wraps admitted entry and ordinary native tools. Both
/// exec in place after capture; existing admission checks still precede payload.
pub fn wrap(client: &mut Command, helper: &Path) -> Result<ClientRecord> {
    let barrier = StartupBarrier::new(format!("amc-start-{}", crate::store::fresh_id()?))?;
    let parent = std::process::id() as i32;
    let start = crate::host_native::process_start(parent)?;
    client.arg("--").arg(helper).args([
        "native-start",
        "--socket",
        barrier.name(),
        "--parent",
        &parent.to_string(),
        "--parent-start",
        &start.to_string(),
        "--",
    ]);
    Ok(ClientRecord::with_startup(barrier))
}

/// Forward `reader` to stderr and report, at EOF, whether `frame` passed through.
fn relay_entry(
    mut reader: impl Read + Send + 'static,
    frame: Vec<u8>,
) -> Result<std::sync::mpsc::Receiver<bool>> {
    let (sender, receiver) = std::sync::mpsc::channel();
    thread::Builder::new()
        .name("amc-stderr-relay".into())
        .spawn(move || {
            let (mut seen, mut window, mut buffer) = (false, Vec::new(), [0; 4096]);
            loop {
                let n = match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                };
                // Keep draining even if the caller closed stderr.
                let _ = std::io::stderr().write_all(&buffer[..n]);
                if !seen {
                    window.extend_from_slice(&buffer[..n]);
                    seen = window.windows(frame.len()).any(|bytes| bytes == frame);
                    window.drain(..window.len().saturating_sub(frame.len() - 1));
                }
            }
            let _ = sender.send(seen);
        })?;
    Ok(receiver)
}

fn execution_runtime(
    contract: &Contract,
    burst: bool,
    requested: Option<u64>,
) -> Result<Option<u64>> {
    ensure!(
        contract.burst == burst,
        "burst flag must match the selected contract"
    );
    if let (Some(requested), Some(maximum)) = (requested, contract.runtime_max_sec) {
        ensure!(requested <= maximum, "requested runtime exceeds contract");
    }
    Ok(requested.or(contract.runtime_max_sec))
}

fn completion_frame(outcome: Outcome, token: &str) -> Option<String> {
    match outcome {
        Outcome::Completed(code) => Some(format!("\x1e{token}:complete:{code}\x1f")),
        _ => None,
    }
}

fn outcome_exit(outcome: Outcome, unit: &str) -> Result<Exit> {
    match outcome {
        Outcome::Completed(code) => Ok(Exit::Code(code)),
        Outcome::Unknown { exit_code, .. } => {
            anyhow::bail!(
                "native outcome unresolved for {unit} (launcher exit {exit_code}); reservation remains coordinator-owned; inspect admission status before retrying"
            )
        }
        Outcome::Cancelled { signal, .. } => Ok(Exit::Signal(signal)),
        Outcome::TimedOut { .. } => {
            anyhow::bail!(
                "native submission timed out for {}; inspect admission status before retrying",
                unit
            )
        }
        _ => {
            anyhow::bail!(
                "native submission {:?} for {}; inspect admission status before retrying",
                outcome,
                unit
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use amc_runner::systemd::Cleanup;

    fn request(mode: Mode, wait: Option<Duration>) -> ExecRequest {
        ExecRequest {
            contract: "nix-eval".into(),
            mode,
            wait,
            runtime_max_sec: None,
            burst: false,
            ready_token: None,
            socket: "/run/user/1000/amc/admission.sock".into(),
            helper: "/run/current-system/sw/bin/amc".into(),
        }
    }

    #[test]
    fn requests_map_to_the_cli_admission_messages() {
        let queued = Mode::Queued {
            memory_max: 16 << 30,
            runtime_max_sec: 86400,
        };
        match request(queued.clone(), None).message() {
            Message::EnqueueWaiting { contract, request } => {
                assert_eq!(contract, "nix-eval");
                assert_eq!(request.memory_max, 16 << 30);
                assert_eq!(request.runtime_max_sec, 86400);
                assert_eq!(request.wait_ms, None);
            }
            other => panic!("{other:?}"),
        }
        match request(queued, Some(Duration::from_secs(5))).message() {
            Message::EnqueueWaiting { request, .. } => assert_eq!(request.wait_ms, Some(5000)),
            other => panic!("{other:?}"),
        }
        match request(Mode::Plain, None).message() {
            Message::Enqueue { wait_ms, .. } => assert_eq!(wait_ms, 120_000),
            other => panic!("{other:?}"),
        }
        match request(
            Mode::Sized { memory_max: 4096 },
            Some(Duration::from_secs(7)),
        )
        .message()
        {
            Message::EnqueueSized {
                wait_ms,
                memory_max,
                ..
            } => assert_eq!((wait_ms, memory_max), (7000, 4096)),
            other => panic!("{other:?}"),
        }
        match request(
            Mode::Bounded {
                memory_max: 4096,
                runtime_max_sec: 30,
            },
            None,
        )
        .message()
        {
            Message::EnqueueBounded {
                wait_ms,
                runtime_max_sec,
                ..
            } => assert_eq!((wait_ms, runtime_max_sec), (120_000, 30)),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn forwarded_environment_applies_overrides_and_excludes_systemd_names() {
        let mut overrides = BTreeMap::new();
        overrides.insert(
            OsString::from("AMC_CLIENT_FIXTURE"),
            Some(OsString::from("1")),
        );
        overrides.insert(OsString::from("NOTIFY_SOCKET"), Some(OsString::from("/x")));
        overrides.insert(OsString::from("PATH"), None);
        let names = forwarded_environment(&overrides);
        assert!(names.iter().any(|name| name == "AMC_CLIENT_FIXTURE"));
        assert!(!names.iter().any(|name| name == "NOTIFY_SOCKET"));
        assert!(!names.iter().any(|name| name == "PATH"));
    }

    #[test]
    fn exits_keep_code_and_signal_provenance() {
        for code in [0, 42, 75, 143, 255] {
            let status = Exit::Code(code).status();
            assert_eq!((status.code(), status.signal()), (Some(code), None));
            assert_eq!(Exit::Code(code).code(), code);
        }
        let status = Exit::Signal(15).status();
        assert_eq!((status.code(), status.signal()), (None, Some(15)));
        assert_eq!(Exit::Signal(15).code(), 143);
    }

    #[test]
    fn removed_variables_are_unset_in_the_unit() {
        let mut overrides = BTreeMap::new();
        overrides.insert(OsString::from("PATH"), None);
        overrides.insert(OsString::from("KEEP"), Some(OsString::from("1")));
        overrides.insert(OsString::from("bad=name"), None);
        assert_eq!(removed_environment(&overrides), ["PATH"]);
    }

    #[test]
    fn collectors_stop_even_while_a_writer_stays_open() {
        let (reader, mut writer) = std::io::pipe().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let handle = collect(reader, stop.clone()).unwrap();
        writer.write_all(b"partial").unwrap();
        thread::sleep(Duration::from_millis(150));
        stop.store(true, Ordering::SeqCst);
        let started = Instant::now();
        assert_eq!(handle.join().unwrap().unwrap(), b"partial");
        assert!(started.elapsed() < Duration::from_secs(1));
        drop(writer);
    }

    #[test]
    fn only_a_relayed_entry_frame_authorizes_completion_proof() {
        let frame = b"\x1e0123456789abcdef0123456789abcdef\x1f".to_vec();
        for (chunks, expected) in [
            (
                vec![
                    &b"waiting\n\x1e0123456789abcdef"[..],
                    &b"0123456789abcdef\x1fout"[..],
                ],
                true,
            ),
            (vec![&b"waiting for host capacity: None\n"[..]], false),
            (
                vec![&b"\x1e0123456789abcdef0123456789abcdef:complete:1\x1f"[..]],
                false,
            ),
        ] {
            let (reader, mut writer) = std::io::pipe().unwrap();
            let seen = relay_entry(reader, frame.clone()).unwrap();
            for chunk in chunks {
                writer.write_all(chunk).unwrap();
            }
            drop(writer);
            assert_eq!(seen.recv_timeout(Duration::from_secs(5)), Ok(expected));
        }
    }

    #[test]
    fn burst_runtime_cannot_grow() {
        let contract: Contract = serde_json::from_value(serde_json::json!({
            "slice": "agent-burst.slice", "memory_max": 536870912, "memory_swap_max": 0,
            "max_running": 2, "pause_file": null, "burst": true, "runtime_max_sec": 5
        }))
        .unwrap();
        assert_eq!(execution_runtime(&contract, true, None).unwrap(), Some(5));
        assert_eq!(
            execution_runtime(&contract, true, Some(2)).unwrap(),
            Some(2)
        );
        assert!(execution_runtime(&contract, true, Some(6)).is_err());
        assert!(execution_runtime(&contract, false, None).is_err());
    }

    #[test]
    fn unresolved_native_outcome_never_reports_success() {
        for cleanup in [Cleanup::Unknown, Cleanup::Stopped] {
            assert!(
                outcome_exit(
                    Outcome::Unknown {
                        exit_code: 0,
                        cleanup
                    },
                    "fixture.service"
                )
                .is_err()
            );
        }
        assert_eq!(
            outcome_exit(Outcome::Completed(0), "fixture.service").unwrap(),
            Exit::Code(0)
        );
        assert_eq!(
            outcome_exit(Outcome::Completed(143), "fixture.service").unwrap(),
            Exit::Code(143)
        );
    }

    #[test]
    fn only_confirmed_workload_results_emit_token_bound_completion_proof() {
        let token = "0123456789abcdef0123456789abcdef";
        for code in [0, 1, 42, 75, 255] {
            assert_eq!(
                completion_frame(Outcome::Completed(code), token),
                Some(format!("\x1e{token}:complete:{code}\x1f"))
            );
        }
        for cleanup in [Cleanup::Unknown, Cleanup::Running, Cleanup::Stopped] {
            for outcome in [
                Outcome::Unknown {
                    exit_code: 0,
                    cleanup,
                },
                Outcome::Unknown {
                    exit_code: 1,
                    cleanup,
                },
                Outcome::TimedOut {
                    started: true,
                    cleanup,
                },
                Outcome::Cancelled {
                    signal: 15,
                    started: true,
                    cleanup,
                },
            ] {
                assert_eq!(completion_frame(outcome, token), None);
            }
        }
    }
}
