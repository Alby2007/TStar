//! The supervisor: owns the runtime process lifecycle (docs/protocol-v1.md
//! §5-6, docs/cancellation.md).
//!
//! Design: a single actor task owns all mutable state (pending calls, streams,
//! liveness). IO is pushed through channels by small spawned tasks — reader,
//! writer, stderr drainer, process monitor — so blocking pipe IO never touches
//! the event loop. Kill semantics are per-platform: unix puts the child in its
//! own process group and killpg's it (catches grandchildren); Windows needs a
//! Job Object (process groups don't exist there).

use std::collections::{HashMap, VecDeque};
use std::io;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command as TokioCommand};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::{interval, sleep, timeout};

use crate::protocol::{self as proto, ErrorObj, InMsg};
use crate::transport::Transport;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Starting,
    Ready,
    Unresponsive,
    Restarting,
    Failed,
    /// Supervisor received `Shutdown` and the runtime exited cleanly.
    /// (Host-side terminal state; not a runtime-reported state.)
    Stopped,
}

#[derive(Debug, Clone)]
pub enum RuntimeEvent {
    StateChanged {
        from: State,
        to: State,
        detail: String,
    },
    /// One line of runtime stderr (always drained — protocol §9).
    Stderr(String),
    /// Parsed `hello` exposes manifest.
    Exposes(Value),
}

#[derive(Debug, Clone)]
pub struct SpawnSpec {
    /// Path to the Python interpreter (system python in dev, bundled in prod).
    pub program: PathBuf,
    /// e.g. `["-m", "tauri_plus", "my_app"]`
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: Option<PathBuf>,
}

impl SpawnSpec {
    pub fn tauri_plus(python: PathBuf, app_module: &str) -> Self {
        Self {
            program: python,
            args: vec!["-m".into(), "tauri_plus".into(), app_module.into()],
            env: vec![],
            cwd: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SupervisorConfig {
    /// Deadline for the runtime's `hello` after spawn.
    pub hello_timeout: Duration,
    /// How often we ping while Ready.
    pub ping_interval: Duration,
    /// Pongs outstanding before we declare the runtime wedged and kill it.
    /// A C extension holding the GIL can starve pongs — see cancellation.md.
    pub missed_pongs_to_kill: u32,
    pub backoff_base: Duration,
    pub backoff_cap: Duration,
    /// Crash-loop breaker: this many crashes within `breaker_window` -> Failed.
    pub breaker_crashes: usize,
    pub breaker_window: Duration,
    /// Grace after `shutdown` request before we kill.
    pub shutdown_grace: Duration,
    /// Stream pull-credit granted to a new streaming call up front.
    pub initial_stream_credit: u64,
    pub max_frame: usize,
}

impl Default for SupervisorConfig {
    fn default() -> Self {
        Self {
            hello_timeout: Duration::from_secs(15),
            ping_interval: Duration::from_secs(5),
            missed_pongs_to_kill: 3,
            backoff_base: Duration::from_millis(250),
            backoff_cap: Duration::from_secs(8),
            breaker_crashes: 5,
            breaker_window: Duration::from_secs(60),
            shutdown_grace: Duration::from_secs(5),
            initial_stream_credit: 64,
            max_frame: proto::max_frame(),
        }
    }
}

/// A streamed item delivered to the caller of `call_stream`.
#[derive(Debug)]
pub enum StreamItem {
    Chunk {
        seq: u64,
        value: Value,
    },
    /// Terminal message for the stream: Ok(result) or Err(error).
    End(Result<Value, ErrorObj>),
}

enum Command {
    Call {
        fn_name: String,
        input: Value,
        respond: oneshot::Sender<Result<Value, ErrorObj>>,
    },
    CallStream {
        fn_name: String,
        input: Value,
        items: mpsc::Sender<StreamItem>,
        task_out: oneshot::Sender<u64>,
    },
    Cancel {
        task: u64,
        respond: oneshot::Sender<bool>,
    },
    GrantCredit {
        task: u64,
        n: u64,
    },
    Shutdown,
}

enum PendingKind {
    Call(oneshot::Sender<Result<Value, ErrorObj>>),
    Stream(mpsc::Sender<StreamItem>),
    Cancel { respond: oneshot::Sender<bool> },
}

/// Handle the app holds; clones freely.
#[derive(Clone)]
pub struct Handle {
    cmd: mpsc::Sender<Command>,
    state: watch::Receiver<State>,
}

impl Handle {
    pub fn state(&self) -> State {
        *self.state.borrow()
    }

    /// Subscribe to state changes (UI lifecycle events).
    pub fn subscribe_state(&self) -> watch::Receiver<State> {
        self.state.clone()
    }

    pub async fn call(&self, fn_name: &str, input: Value) -> Result<Value, ErrorObj> {
        let (tx, rx) = oneshot::channel();
        self.cmd
            .send(Command::Call {
                fn_name: fn_name.into(),
                input,
                respond: tx,
            })
            .await
            .map_err(|_| stopped_err())?;
        rx.await.map_err(|_| stopped_err())?
    }

    /// Start a streaming call; yields chunks until `StreamItem::End`.
    pub async fn call_stream(
        &self,
        fn_name: &str,
        input: Value,
    ) -> Result<(u64, mpsc::Receiver<StreamItem>), ErrorObj> {
        let (items_tx, items_rx) = mpsc::channel(64);
        let (task_tx, task_rx) = oneshot::channel();
        self.cmd
            .send(Command::CallStream {
                fn_name: fn_name.into(),
                input,
                items: items_tx,
                task_out: task_tx,
            })
            .await
            .map_err(|_| stopped_err())?;
        let task = task_rx.await.map_err(|_| stopped_err())?;
        Ok((task, items_rx))
    }

    /// Ask the supervisor to cancel `task`. Resolves to the runtime's ack.
    pub async fn cancel(&self, task: u64) -> Result<bool, ErrorObj> {
        let (tx, rx) = oneshot::channel();
        self.cmd
            .send(Command::Cancel { task, respond: tx })
            .await
            .map_err(|_| stopped_err())?;
        rx.await.map_err(|_| stopped_err())
    }

    /// Grant more stream pull-credit to `task`.
    pub async fn grant_credit(&self, task: u64, n: u64) -> Result<(), ErrorObj> {
        self.cmd
            .send(Command::GrantCredit { task, n })
            .await
            .map_err(|_| stopped_err())
    }

    pub async fn shutdown(&self) {
        let _ = self.cmd.send(Command::Shutdown).await;
    }
}

fn stopped_err() -> ErrorObj {
    ErrorObj {
        code: proto::RUNTIME_UNAVAILABLE,
        message: "runtime is stopped".into(),
        data: None,
    }
}

/// Spawn the supervisor actor. `events` receives lifecycle/log events.
pub fn spawn(
    spec: SpawnSpec,
    config: SupervisorConfig,
    events: mpsc::Sender<RuntimeEvent>,
) -> Handle {
    let (cmd_tx, cmd_rx) = mpsc::channel(64);
    let (state_tx, state_rx) = watch::channel(State::Starting);
    tokio::spawn(
        Supervisor {
            spec,
            config,
            events,
            cmd_rx,
            state_tx,
            generation: 0,
            next_id: AtomicU64::new(1),
            next_task: AtomicU64::new(1),
        }
        .run(),
    );
    Handle {
        cmd: cmd_tx,
        state: state_rx,
    }
}

struct Supervisor {
    spec: SpawnSpec,
    config: SupervisorConfig,
    events: mpsc::Sender<RuntimeEvent>,
    cmd_rx: mpsc::Receiver<Command>,
    state_tx: watch::Sender<State>,
    generation: u64,
    next_id: AtomicU64,
    next_task: AtomicU64,
}

/// Per-child IO plumbing, owned by the per-spawn scope.
struct ChildIo {
    /// Push raw frame bodies to write to child stdin.
    frame_out: mpsc::Sender<Vec<u8>>,
    /// Received decoded incoming messages / transport errors.
    msg_in: mpsc::Receiver<Result<InMsg, ChildReadErr>>,
    /// Exit status of the child process.
    exited: oneshot::Receiver<io::Result<std::process::ExitStatus>>,
    /// Signal the monitor to kill the child process tree.
    kill: mpsc::Sender<()>,
    // stderr is drained by a detached task -> events.
}

enum ChildReadErr {
    Io(io::Error),
    Malformed(serde_json::Error),
}

impl std::fmt::Display for ChildReadErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "io: {e}"),
            Self::Malformed(e) => write!(f, "malformed frame: {e}"),
        }
    }
}

enum ExitCause {
    /// `shutdown` requested and honoured (or stdin-EOF exit) — stop cleanly.
    CleanShutdown,
    /// Runtime exited or became unusable unexpectedly.
    Crashed(String),
}

impl Supervisor {
    fn set_state(&self, to: State, detail: impl Into<String>) {
        let from = *self.state_tx.borrow();
        if from != to {
            let _ = self.state_tx.send(to);
            let _ = self.events.try_send(RuntimeEvent::StateChanged {
                from,
                to,
                detail: detail.into(),
            });
        }
    }

    async fn run(mut self) {
        let mut crashes: VecDeque<Instant> = VecDeque::new();
        let mut backoff = self.config.backoff_base;
        loop {
            self.set_state(
                State::Starting,
                format!("spawning (gen {})", self.generation),
            );
            let cause = match spawn_child(&self.spec, &self.config, &self.events).await {
                Ok(io) => self.run_child(io).await,
                Err(e) => ExitCause::Crashed(format!("spawn failed: {e}")),
            };

            match cause {
                ExitCause::CleanShutdown => {
                    self.set_state(State::Stopped, "shutdown");
                    return;
                }
                ExitCause::Crashed(reason) => {
                    let now = Instant::now();
                    while crashes
                        .front()
                        .is_some_and(|t| now.duration_since(*t) > self.config.breaker_window)
                    {
                        crashes.pop_front();
                    }
                    crashes.push_back(now);
                    if crashes.len() >= self.config.breaker_crashes {
                        self.set_state(
                            State::Failed,
                            format!(
                                "crash loop: {reason} ({} crashes in {:?})",
                                crashes.len(),
                                self.config.breaker_window
                            ),
                        );
                        return; // circuit open — stays Failed until restart()
                    }
                    self.set_state(State::Restarting, reason);
                    sleep(jitter(backoff)).await;
                    backoff = (backoff * 2).min(self.config.backoff_cap);
                    self.generation += 1;
                }
            }
        }
    }

    /// One runtime lifecycle: handshake, then serve until exit/kill.
    async fn run_child(&mut self, mut io: ChildIo) -> ExitCause {
        // ---- handshake: hello within deadline ----
        let hello = timeout(self.config.hello_timeout, io.msg_in.recv()).await;
        let hello = match hello {
            Ok(Some(Ok(InMsg::Notification { method, params }))) if method == "hello" => {
                params.unwrap_or(Value::Null)
            }
            Ok(Some(Ok(_))) => {
                kill_and_reap(&mut io).await;
                return ExitCause::Crashed("first frame was not hello".into());
            }
            Ok(Some(Err(e))) => {
                kill_and_reap(&mut io).await;
                return ExitCause::Crashed(format!("protocol violation during hello: {e}"));
            }
            Ok(None) | Err(_) => {
                kill_and_reap(&mut io).await;
                return ExitCause::Crashed(format!(
                    "no hello within {:?} (startup hang or early exit)",
                    self.config.hello_timeout
                ));
            }
        };

        let version_ok = hello["protocol"].as_u64() == Some(proto::PROTOCOL_VERSION as u64);
        if !version_ok {
            kill_and_reap(&mut io).await;
            return ExitCause::Crashed(format!(
                "protocol version mismatch: {:?}",
                hello["protocol"]
            ));
        }
        let _ = self
            .events
            .try_send(RuntimeEvent::Exposes(hello["exposes"].clone()));
        self.set_state(State::Ready, "handshake complete");

        // ---- serving ----
        let mut pending: HashMap<u64, PendingKind> = HashMap::new();
        let mut task_of_call: HashMap<u64, u64> = HashMap::new(); // call_id -> task
        let mut call_of_task: HashMap<u64, u64> = HashMap::new(); // task -> call_id
        let mut pongs_outstanding: u32 = 0;
        let mut ping_seq: u64 = 0;
        let mut ping_tick = interval(self.config.ping_interval);
        ping_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                // --- incoming frame ---
                msg = io.msg_in.recv() => {
                    match msg {
                        Some(Ok(InMsg::Response { id, result, error })) => {
                            let out = match (result, error) {
                                (_, Some(e)) => Err(e),
                                (r, None) => Ok(r.unwrap_or(Value::Null)),
                            };
                            if let Some(kind) = pending.remove(&id) {
                                match kind {
                                    PendingKind::Call(tx) => { let _ = tx.send(out); }
                                    PendingKind::Stream(tx) => {
                                        let _ = tx.send(StreamItem::End(out)).await;
                                    }
                                    PendingKind::Cancel { respond } => {
                                        let cancelled = out
                                            .as_ref()
                                            .ok()
                                            .and_then(|v| v["cancelled"].as_bool())
                                            .unwrap_or(false);
                                        let _ = respond.send(cancelled);
                                    }
                                }
                            }
                            if let Some(task) = task_of_call.remove(&id) {
                                call_of_task.remove(&task);
                            }
                        }
                        Some(Ok(InMsg::Notification { method, params })) => {
                            let p = params.unwrap_or(Value::Null);
                            match method.as_str() {
                                "pong" => {
                                    pongs_outstanding = 0;
                                    self.set_state(State::Ready, "pong received");
                                }
                                "stream.chunk" => {
                                    let task = p["task"].as_u64().unwrap_or(0);
                                    if let Some(&call_id) = call_of_task.get(&task) {
                                        if let Some(PendingKind::Stream(tx)) = pending.get(&call_id) {
                                            let _ = tx.try_send(StreamItem::Chunk {
                                                seq: p["seq"].as_u64().unwrap_or(0),
                                                value: p["value"].clone(),
                                            });
                                        }
                                    }
                                }
                                _ => {} // unknown notifications are ignored (spec §8)
                            }
                        }
                        Some(Err(e)) => {
                            fail_all(&mut pending, proto::PROTOCOL_VIOLATION, "runtime sent a malformed frame").await;
                            kill_and_reap(&mut io).await;
                            return ExitCause::Crashed(format!("protocol violation: {e}"));
                        }
                        None => {
                            // reader hit EOF: runtime is gone; monitor should confirm
                            let detail = match timeout(Duration::from_secs(5), &mut io.exited).await {
                                Ok(Ok(Ok(status))) => format!("exited {status}"),
                                _ => "runtime gone".into(),
                            };
                            fail_all(&mut pending, proto::RUNTIME_CRASHED, &detail).await;
                            return ExitCause::Crashed(detail);
                        }
                    }
                }
                // --- app command ---
                cmd = self.cmd_rx.recv() => {
                    let Some(cmd) = cmd else {
                        // all handles dropped — shut down
                        kill_and_reap(&mut io).await;
                        return ExitCause::CleanShutdown;
                    };
                    match cmd {
                        Command::Call { fn_name, input, respond } => {
                            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
                            let task = self.next_task.fetch_add(1, Ordering::Relaxed);
                            let body = proto::request(id, "call",
                                serde_json::json!({"fn": fn_name, "task": task, "input": input}));
                            if send_frame(&io, &body) {
                                pending.insert(id, PendingKind::Call(respond));
                                task_of_call.insert(id, task);
                                call_of_task.insert(task, id);
                            } else {
                                let _ = respond.send(Err(ErrorObj {
                                    code: proto::RUNTIME_CRASHED,
                                    message: "runtime stdin is gone".into(), data: None }));
                            }
                        }
                        Command::CallStream { fn_name, input, items, task_out } => {
                            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
                            let task = self.next_task.fetch_add(1, Ordering::Relaxed);
                            let body = proto::request(id, "call",
                                serde_json::json!({"fn": fn_name, "task": task, "input": input}));
                            let credit = proto::notification("stream.credit",
                                serde_json::json!({"task": task, "n": self.config.initial_stream_credit}));
                            if send_frame(&io, &body) {
                                let _ = task_out.send(task);
                                send_frame(&io, &credit);
                                pending.insert(id, PendingKind::Stream(items));
                                task_of_call.insert(id, task);
                                call_of_task.insert(task, id);
                            } else {
                                let _ = items.send(StreamItem::End(Err(stopped_err()))).await;
                            }
                        }
                        Command::Cancel { task, respond } => {
                            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
                            let body = proto::request(id, "cancel",
                                serde_json::json!({"task": task}));
                            if send_frame(&io, &body) {
                                pending.insert(id, PendingKind::Cancel { respond });
                            } else {
                                let _ = respond.send(false);
                            }
                        }
                        Command::GrantCredit { task, n } => {
                            let body = proto::notification("stream.credit",
                                serde_json::json!({"task": task, "n": n}));
                            send_frame(&io, &body);
                        }
                        Command::Shutdown => {
                            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
                            let body = proto::request(id, "shutdown", Value::Null);
                            send_frame(&io, &body);
                            let _ = timeout(self.config.shutdown_grace, &mut io.exited).await;
                            kill_and_reap(&mut io).await;
                            fail_all(&mut pending, proto::RUNTIME_UNAVAILABLE, "shutdown").await;
                            return ExitCause::CleanShutdown;
                        }
                    }
                }
                // --- liveness ping ---
                _ = ping_tick.tick() => {
                    ping_seq += 1;
                    pongs_outstanding += 1;
                    if pongs_outstanding >= self.config.missed_pongs_to_kill {
                        self.set_state(State::Unresponsive,
                            format!("{} missed pongs — runtime wedged", pongs_outstanding));
                        fail_all(&mut pending, proto::RUNTIME_WEDGED,
                            "runtime stopped responding (possible native-code wedge)").await;
                        kill_and_reap(&mut io).await;
                        return ExitCause::Crashed(format!(
                            "wedged: {} missed pongs", pongs_outstanding));
                    }
                    self.set_state(State::Unresponsive,
                        format!("missed {} pong(s)", pongs_outstanding));
                    let body = proto::request(self.next_id.fetch_add(1, Ordering::Relaxed),
                        "ping", serde_json::json!({"seq": ping_seq}));
                    send_frame(&io, &body);
                }
                // --- child exit ---
                status = &mut io.exited => {
                    let detail = match status {
                        Ok(Ok(s)) => format!("exited {s}"),
                        Ok(Err(e)) => format!("wait error: {e}"),
                        Err(_) => "monitor dropped".into(),
                    };
                    fail_all(&mut pending, proto::RUNTIME_CRASHED, &detail).await;
                    return ExitCause::Crashed(detail);
                }
            }
        }
    }
}

fn send_frame(io: &ChildIo, body: &Value) -> bool {
    match serde_json::to_vec(body) {
        Ok(bytes) => io.frame_out.try_send(bytes).is_ok(),
        Err(_) => false,
    }
}

async fn fail_all(pending: &mut HashMap<u64, PendingKind>, code: i64, msg: &str) {
    for (_, kind) in pending.drain() {
        let err = ErrorObj {
            code,
            message: msg.into(),
            data: None,
        };
        match kind {
            PendingKind::Call(tx) => {
                let _ = tx.send(Err(err));
            }
            PendingKind::Stream(tx) => {
                let _ = tx.send(StreamItem::End(Err(err))).await;
            }
            PendingKind::Cancel { respond } => {
                let _ = respond.send(false);
            }
        }
    }
}

async fn kill_and_reap(io: &mut ChildIo) {
    let _ = io.kill.try_send(());
}

fn jitter(d: Duration) -> Duration {
    // +/-25% jitter, deterministic-enough without an RNG dep
    let nanos = d.as_nanos() as u64;
    let j = nanos / 4;
    let r = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|t| t.subsec_nanos() as u64)
        .unwrap_or(0))
        % (2 * j + 1);
    Duration::from_nanos(nanos - j + r)
}

// ---- process spawning & kill plumbing ----

async fn spawn_child(
    spec: &SpawnSpec,
    config: &SupervisorConfig,
    events: &mpsc::Sender<RuntimeEvent>,
) -> io::Result<ChildIo> {
    let mut cmd = TokioCommand::new(&spec.program);
    cmd.args(&spec.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(cwd) = &spec.cwd {
        cmd.current_dir(cwd);
    }
    cmd.envs(spec.env.iter().cloned());
    #[cfg(unix)]
    {
        // Own process group so killpg can reach grandchildren (protocol §10).
        cmd.process_group(0);
    }

    let mut child = cmd.spawn()?;
    #[cfg(windows)]
    let pid = child.id();
    let stdin = child.stdin.take().expect("piped");
    let stdout = child.stdout.take().expect("piped");
    let stderr = child.stderr.take().expect("piped");

    // Job Object handle must outlive the child — held by the monitor task.
    #[cfg(windows)]
    let job = assign_to_job_object(pid);

    // monitor: owns the child, awaits exit or kill signal
    let (exit_tx, exit_rx) = oneshot::channel();
    let (kill_tx, kill_rx) = mpsc::channel::<()>(1);
    #[cfg(windows)]
    tokio::spawn(monitor_child(child, kill_rx, exit_tx, job));
    #[cfg(not(windows))]
    tokio::spawn(monitor_child(child, kill_rx, exit_tx));

    // writer: frames -> child stdin
    let (frame_tx, mut frame_rx) = mpsc::channel::<Vec<u8>>(64);
    tokio::spawn(async move {
        let mut w = stdin;
        while let Some(body) = frame_rx.recv().await {
            use tokio::io::AsyncWriteExt;
            if w.write_all(&(body.len() as u32).to_be_bytes())
                .await
                .is_err()
                || w.write_all(&body).await.is_err()
                || w.flush().await.is_err()
            {
                return; // child stdin gone
            }
        }
    });

    // reader: child stdout -> parsed messages
    let (msg_tx, msg_rx) = mpsc::channel::<Result<InMsg, ChildReadErr>>(64);
    let max_frame = config.max_frame;
    tokio::spawn(async move {
        let mut t = ReaderOnly {
            inner: stdout,
            max_frame,
        };
        loop {
            match t.read_frame().await {
                Ok(Some(body)) => {
                    let parsed = InMsg::parse(&body).map_err(ChildReadErr::Malformed);
                    if msg_tx.send(parsed).await.is_err() {
                        return;
                    }
                }
                Ok(None) => return, // EOF
                Err(e) => {
                    let _ = msg_tx.send(Err(ChildReadErr::Io(e))).await;
                    return;
                }
            }
        }
    });

    // stderr: always drained, forwarded as events (protocol §9)
    let ev = events.clone();
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if ev.try_send(RuntimeEvent::Stderr(line)).is_err() {
                // channel full — drop oldest policy is the sink's problem;
                // we must keep draining regardless to avoid pipe deadlock
            }
        }
    });

    Ok(ChildIo {
        frame_out: frame_tx,
        msg_in: msg_rx,
        exited: exit_rx,
        kill: kill_tx,
    })
}

/// stdout reader without a writer — the reader task only consumes.
struct ReaderOnly<R> {
    inner: R,
    max_frame: usize,
}

#[async_trait::async_trait]
impl<R: tokio::io::AsyncRead + Unpin + Send> Transport for ReaderOnly<R> {
    async fn read_frame(&mut self) -> io::Result<Option<Vec<u8>>> {
        use tokio::io::AsyncReadExt;
        let mut len_buf = [0u8; 4];
        match self.inner.read_exact(&mut len_buf).await {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e),
        }
        let n = u32::from_be_bytes(len_buf) as usize;
        if n > self.max_frame {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                crate::transport::FrameTooLarge(n),
            ));
        }
        let mut body = vec![0u8; n];
        self.inner.read_exact(&mut body).await?;
        Ok(Some(body))
    }
    async fn write_frame(&mut self, _body: &[u8]) -> io::Result<()> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "reader only"))
    }
}

#[cfg(not(windows))]
async fn monitor_child(
    mut child: Child,
    mut kill_rx: mpsc::Receiver<()>,
    exit_tx: oneshot::Sender<io::Result<std::process::ExitStatus>>,
) {
    tokio::select! {
        status = child.wait() => { let _ = exit_tx.send(status); }
        _ = kill_rx.recv() => {
            kill_tree(&mut child).await;
            let status = child.wait().await;
            let _ = exit_tx.send(status);
        }
    }
}

#[cfg(windows)]
async fn monitor_child(
    mut child: Child,
    mut kill_rx: mpsc::Receiver<()>,
    exit_tx: oneshot::Sender<io::Result<std::process::ExitStatus>>,
    // Holding this handle keeps the Job Object alive; closing it (drop at
    // task end) after the child exits is harmless.
    _job: Option<windows_sys::Win32::Foundation::HANDLE>,
) {
    tokio::select! {
        status = child.wait() => { let _ = exit_tx.send(status); }
        _ = kill_rx.recv() => {
            let _ = child.start_kill(); // Job Object also reaps on handle close
            let status = child.wait().await;
            let _ = exit_tx.send(status);
        }
    }
}

/// Kill the child process tree: killpg on unix (own process group, protocol
/// §10), Job Object on Windows, falling back to plain child kill.
async fn kill_tree(child: &mut Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        unsafe {
            // negative pid = process group we created via process_group(0)
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
    let _ = child.start_kill();
}

#[cfg(windows)]
fn assign_to_job_object(pid: Option<u32>) -> Option<windows_sys::Win32::Foundation::HANDLE> {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::*;
    use windows_sys::Win32::System::Threading::*;

    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job == 0 {
            return None;
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const _,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        ) == 0
        {
            CloseHandle(job);
            return None;
        }
        let Some(pid) = pid else {
            CloseHandle(job);
            return None;
        };
        let proc = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid);
        if proc == 0 || AssignProcessToJobObject(job, proc) == 0 {
            if proc != 0 {
                CloseHandle(proc);
            }
            CloseHandle(job);
            return None;
        }
        CloseHandle(proc);
        Some(job as HANDLE) // kept alive by caller for the child's lifetime
    }
}
