//! The supervisor: owns the runtime process lifecycle (docs/protocol-v1.md
//! §5-6, docs/cancellation.md).
//!
//! Design: a single actor task owns all mutable state (pending calls, streams,
//! liveness). IO is pushed through channels by small spawned tasks — reader,
//! writer, stderr drainer, process monitor — so blocking pipe IO never touches
//! the event loop. Kill semantics are per-platform: unix puts the child in its
//! own process group and killpg's it (catches grandchildren); Windows needs a
//! Job Object (process groups don't exist there).

use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command as TokioCommand};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::{interval, timeout};

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

#[derive(Debug, Clone, Default)]
pub struct SpawnSpec {
    /// Path to the Python interpreter (system python in dev, bundled in prod).
    pub program: PathBuf,
    /// e.g. `["-m", "pylair", "my_app"]`
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    /// Variables stripped from the INHERITED environment before `env` is
    /// applied (e.g. a launcher's PYTHONHOME must not reach a bundled Python).
    pub env_remove: Vec<String>,
    pub cwd: Option<PathBuf>,
}

impl SpawnSpec {
    pub fn pylair(python: PathBuf, app_module: &str) -> Self {
        Self {
            program: python,
            args: vec!["-m".into(), "pylair".into(), app_module.into()],
            env: vec![],
            env_remove: vec![],
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
    /// Chunks buffered per stream between the supervisor and its consumer.
    /// A consumer that falls this far behind is cut off (and its producer
    /// cancelled) rather than allowed to stall the supervisor.
    pub stream_buffer: usize,
    /// Pongs come from a reader thread and only prove the process and pipe
    /// are alive. The runtime also reports how long its asyncio loop has made
    /// no progress (`loop_stale_ms`). `None` (the default) reports stalls as
    /// a diagnostic only — legitimate blocking work (a multi-GB model load
    /// inside `async def`) can stall the loop for minutes. `Some(limit)`
    /// treats a stall at or beyond it as wedged: in-flight calls fail with
    /// `RUNTIME_WEDGED` and the runtime is killed and restarted.
    pub loop_stall_kill: Option<Duration>,
    /// How long `cancel()` waits for the runtime's acknowledgement.
    pub cancel_timeout: Duration,
    pub max_frame: usize,
    /// Dev-mode hot reload: when this file's mtime changes, respawn the
    /// runtime (no crash accounting). Falls back to the
    /// PYLAIR_RESTART_TRIGGER env var when unset.
    pub restart_trigger: Option<PathBuf>,
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
            stream_buffer: 256,
            loop_stall_kill: None,
            cancel_timeout: Duration::from_secs(5),
            max_frame: proto::max_frame(),
            restart_trigger: None,
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

/// Supervisor-side end of a stream. Chunks are bounded (`try_send`); the
/// terminal result travels on its own one-shot, so it can never be lost to a
/// full chunk buffer and delivering it never awaits.
struct StreamSink {
    chunks: mpsc::Sender<StreamItem>,
    end: Option<oneshot::Sender<Result<Value, ErrorObj>>>,
}

/// Why a chunk could not be delivered.
enum ChunkFail {
    /// Buffer full: the consumer is not draining.
    Stalled,
    /// Receiver dropped: the consumer is gone.
    Gone,
}

impl StreamSink {
    fn chunk(&self, seq: u64, value: Value) -> Result<(), ChunkFail> {
        self.chunks
            .try_send(StreamItem::Chunk { seq, value })
            .map_err(|e| match e {
                mpsc::error::TrySendError::Full(_) => ChunkFail::Stalled,
                mpsc::error::TrySendError::Closed(_) => ChunkFail::Gone,
            })
    }

    /// Deliver the terminal. Never blocks; dropping the sink afterwards closes
    /// the chunk channel, which is how the receiver learns to read the end.
    fn finish(mut self, result: Result<Value, ErrorObj>) {
        if let Some(end) = self.end.take() {
            let _ = end.send(result);
        }
    }
}

/// Consumer-side end of a stream: queued chunks first, then exactly one
/// `StreamItem::End`, then `None`.
pub struct StreamRx {
    chunks: mpsc::Receiver<StreamItem>,
    end: Option<oneshot::Receiver<Result<Value, ErrorObj>>>,
}

impl StreamRx {
    pub async fn recv(&mut self) -> Option<StreamItem> {
        if self.end.is_some() {
            if let Some(item) = self.chunks.recv().await {
                return Some(item);
            }
            // Chunk channel closed => the sink is gone and the terminal is set
            // (or the supervisor itself dropped).
            let end = self.end.take()?;
            return Some(StreamItem::End(
                end.await.unwrap_or_else(|_| Err(stopped_err())),
            ));
        }
        None
    }
}

fn stream_pair(buffer: usize) -> (StreamSink, StreamRx) {
    let (ctx, crx) = mpsc::channel(buffer.max(1));
    let (etx, erx) = oneshot::channel();
    (
        StreamSink {
            chunks: ctx,
            end: Some(etx),
        },
        StreamRx {
            chunks: crx,
            end: Some(erx),
        },
    )
}

enum Command {
    Call {
        fn_name: String,
        input: Value,
        respond: oneshot::Sender<Result<Value, ErrorObj>>,
        task_out: oneshot::Sender<u64>,
        owner: Option<String>,
    },
    CallStream {
        fn_name: String,
        input: Value,
        items: StreamSink,
        task_out: oneshot::Sender<u64>,
        owner: Option<String>,
    },
    Cancel {
        task: u64,
        respond: oneshot::Sender<CancelOutcome>,
        caller: Option<String>,
    },
    GrantCredit {
        task: u64,
        n: u64,
        respond: oneshot::Sender<Result<(), ErrorObj>>,
        caller: Option<String>,
    },
    /// Cancel every task owned by this webview label — sent when the
    /// window is destroyed (supervisor-initiated, not a JS caller).
    CancelOwned { owner: String },
    /// Operator-initiated restart out of the Failed state.
    Retry,
    /// Stop the runtime. `ack` fires only after the process tree is reaped,
    /// carrying what cleanup actually achieved.
    Shutdown {
        ack: oneshot::Sender<ShutdownOutcome>,
    },
}

/// What happened to a cancel request — distinct outcomes so a caller never
/// has to guess whether its frame landed or its task stopped (RC.6B).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CancelOutcome {
    /// Runtime confirmed the task stopped.
    Cancelled,
    /// Delivered, but the work can't be interrupted (inline sync): the
    /// caller sees TASK_CANCELLED while the Python may run to completion.
    RequestedMayContinue,
    /// Isolated worker was terminated and its process reaped.
    Terminated,
    /// Termination was requested but couldn't be confirmed — the worker
    /// may still be running.
    TerminationUncertain,
    /// The task already finished before the cancel landed.
    Completed,
    /// No such task in this runtime generation (stale or unknown id).
    NotFound,
    /// The caller does not own this task.
    Rejected,
    /// The control frame could not be queued — nothing was delivered.
    DeliveryFailed,
}

/// What shutdown actually achieved — reported, never assumed (RC.6C).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ShutdownOutcome {
    /// Graceful exit inside the grace window.
    Terminated,
    /// Had to SIGKILL/taskkill — the tree is confirmed dead.
    ForcedKill,
    /// Kill sent but the exit wasn't confirmed in the wait window.
    Incomplete,
    /// The runtime was already gone or its state unknown.
    Unknown,
}

/// Stale-loop duration at which pongs surface an `Unresponsive` diagnostic.
/// Below the Python lag monitor's 1s threshold this would flap on jitter.
const LOOP_STALL_WARN: Duration = Duration::from_secs(1);

/// What a pong's `loop_stale_ms` means for the runtime. Pure so the
/// verdict boundaries are unit-testable without a clock or a process:
/// a configured kill wins over the warning, and with no kill configured
/// a stalled loop is only ever a diagnostic (P1-7: model loads can block
/// the loop for minutes on a healthy app).
#[derive(Debug, PartialEq, Eq)]
enum PongVerdict {
    Ready,
    StallWarn,
    StallKill,
}

fn classify_pong(stale: Duration, kill: Option<Duration>) -> PongVerdict {
    if kill.is_some_and(|l| stale >= l) {
        PongVerdict::StallKill
    } else if stale >= LOOP_STALL_WARN {
        PongVerdict::StallWarn
    } else {
        PongVerdict::Ready
    }
}

/// Per-task metadata for ownership and completion tracking (RC.6A).
struct TaskMeta {
    call_id: u64,
    owner: Option<String>,
}

enum PendingKind {
    Call(oneshot::Sender<Result<Value, ErrorObj>>),
    Stream(StreamSink),
    Cancel {
        respond: oneshot::Sender<CancelOutcome>,
    },
}

/// An eagerly-dispatched call: the request is already on its way when this
/// returns (unlike `call`, an async fn which is lazy until polled). `task`
/// resolves to the runtime task id — needed for `cancel`.
pub struct CallTask {
    pub task: oneshot::Receiver<u64>,
    pub result: oneshot::Receiver<Result<Value, ErrorObj>>,
}

/// Handle the app holds; clones freely.
#[derive(Clone)]
pub struct Handle {
    cmd: mpsc::Sender<Command>,
    state: watch::Receiver<State>,
    stream_buffer: usize,
    cancel_timeout: Duration,
}

impl Handle {
    pub fn state(&self) -> State {
        *self.state.borrow()
    }

    /// Subscribe to state changes (UI lifecycle events).
    pub fn subscribe_state(&self) -> watch::Receiver<State> {
        self.state.clone()
    }

    /// Dispatch a call immediately (non-async, try_send) and get its task id.
    /// `owner` is the controlling caller's identity (a webview label from
    /// Tauri — never a JS-supplied value); `None` marks the task unowned
    /// (test/dev paths only — plugin commands always pass the label).
    pub fn call_task(
        &self,
        fn_name: &str,
        input: Value,
        owner: Option<String>,
    ) -> Result<CallTask, ErrorObj> {
        let (rtx, rrx) = oneshot::channel();
        let (ttx, trx) = oneshot::channel();
        self.cmd
            .try_send(Command::Call {
                fn_name: fn_name.into(),
                input,
                respond: rtx,
                task_out: ttx,
                owner,
            })
            .map_err(|_| stopped_err())?;
        Ok(CallTask {
            task: trx,
            result: rrx,
        })
    }

    /// Call a function and await its result. For cancellation, prefer
    /// `call_task` — a pending `call` future carries no task id.
    pub async fn call(
        &self,
        fn_name: &str,
        input: Value,
        owner: Option<String>,
    ) -> Result<Value, ErrorObj> {
        let ct = self.call_task(fn_name, input, owner)?;
        ct.result.await.map_err(|_| stopped_err())?
    }

    /// Start a streaming call; yields chunks until `StreamItem::End`.
    pub async fn call_stream(
        &self,
        fn_name: &str,
        input: Value,
        owner: Option<String>,
    ) -> Result<(u64, StreamRx), ErrorObj> {
        let (items_tx, items_rx) = stream_pair(self.stream_buffer);
        let (task_tx, task_rx) = oneshot::channel();
        self.cmd
            .send(Command::CallStream {
                fn_name: fn_name.into(),
                input,
                items: items_tx,
                task_out: task_tx,
                owner,
            })
            .await
            .map_err(|_| stopped_err())?;
        let task = task_rx.await.map_err(|_| stopped_err())?;
        Ok((task, items_rx))
    }

    /// Retry after a Failed state — respawns the runtime fresh.
    pub async fn retry(&self) -> Result<(), ErrorObj> {
        self.cmd
            .send(Command::Retry)
            .await
            .map_err(|_| stopped_err())
    }

    /// Ask the supervisor to cancel `task` as `caller`. Resolves to a
    /// `CancelOutcome` — never an ambiguous bool (RC.6B).
    pub async fn cancel(
        &self,
        task: u64,
        caller: Option<String>,
    ) -> Result<CancelOutcome, ErrorObj> {
        let (tx, rx) = oneshot::channel();
        self.cmd
            .send(Command::Cancel {
                task,
                respond: tx,
                caller,
            })
            .await
            .map_err(|_| stopped_err())?;
        match timeout(self.cancel_timeout, rx).await {
            Ok(r) => r.map_err(|_| stopped_err()),
            Err(_) => Err(ErrorObj {
                code: proto::RUNTIME_WEDGED,
                message: format!(
                    "cancel not acknowledged within {:?}: the runtime's event loop may be blocked",
                    self.cancel_timeout
                ),
                data: None,
            }),
        }
    }

    /// Cancel every task owned by `owner` — used when a window is destroyed.
    /// Delivers from a spawned task so a momentarily-full command queue
    /// cannot silently drop it (the window's tasks would run on orphaned).
    pub fn cancel_owner(&self, owner: String) {
        let cmd = self.cmd.clone();
        tokio::spawn(async move {
            let _ = cmd.send(Command::CancelOwned { owner }).await;
        });
    }

    /// Grant more stream pull-credit to `task` as `caller`.
    pub async fn grant_credit(
        &self,
        task: u64,
        n: u64,
        caller: Option<String>,
    ) -> Result<(), ErrorObj> {
        let (tx, rx) = oneshot::channel();
        self.cmd
            .send(Command::GrantCredit {
                task,
                n,
                respond: tx,
                caller,
            })
            .await
            .map_err(|_| stopped_err())?;
        rx.await.map_err(|_| stopped_err())?
    }

    /// Stop the runtime and wait until its process tree is gone (or the
    /// supervisor has already stopped). Returns what cleanup achieved.
    pub async fn shutdown(&self) -> ShutdownOutcome {
        let (ack, done) = oneshot::channel();
        if self.cmd.send(Command::Shutdown { ack }).await.is_ok() {
            done.await.unwrap_or(ShutdownOutcome::Unknown)
        } else {
            ShutdownOutcome::Unknown
        }
    }
}

fn stopped_err() -> ErrorObj {
    ErrorObj {
        code: proto::RUNTIME_UNAVAILABLE,
        message: "runtime is stopped".into(),
        data: None,
    }
}

/// Create the supervisor actor. Returns the handle plus the actor future —
/// the CALLER drives it (tokio::spawn / tauri::async_runtime::spawn) because
/// bare `tokio::spawn` panics outside an entered runtime, and plugin `setup`
/// runs on the main thread where no reactor is entered.
pub fn spawn(
    spec: SpawnSpec,
    config: SupervisorConfig,
    events: mpsc::Sender<RuntimeEvent>,
) -> (
    Handle,
    impl std::future::Future<Output = ()> + Send + 'static,
) {
    let (cmd_tx, cmd_rx) = mpsc::channel(64);
    let (state_tx, state_rx) = watch::channel(State::Starting);
    let stream_buffer = config.stream_buffer;
    let cancel_timeout = config.cancel_timeout;
    let actor = Supervisor {
        spec,
        config,
        events,
        cmd_rx,
        state_tx,
        generation: 0,
        next_id: AtomicU64::new(1),
        next_task: AtomicU64::new(1),
        ready_since: None,
        shutdown_acks: Vec::new(),
        shutdown_outcome: None,
    }
    .run();
    (
        Handle {
            cmd: cmd_tx,
            state: state_rx,
            stream_buffer,
            cancel_timeout,
        },
        actor,
    )
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
    /// When the current generation reached Ready — a crash after a sustained
    /// ready period is a fresh incident, not a boot loop.
    ready_since: Option<Instant>,
    /// Waiters for a completed shutdown, released when the actor stops.
    shutdown_acks: Vec<oneshot::Sender<ShutdownOutcome>>,
    /// What the most recent shutdown actually achieved (RC.6C).
    shutdown_outcome: Option<ShutdownOutcome>,
}

/// Per-child IO plumbing, owned by the per-spawn scope.
struct ChildIo {
    /// Push raw frame bodies to write to child stdin.
    frame_out: mpsc::Sender<Vec<u8>>,
    /// Control frames (cancel / credit / shutdown) — a separate bounded
    /// lane the writer task always drains first, so a data flood cannot
    /// starve or silently drop control (RC.6B).
    ctrl_out: mpsc::Sender<Vec<u8>>,
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
    /// Runtime exited or became unusable unexpectedly — crash accounting applies.
    Crashed(String),
    /// Unrecoverable without operator action (e.g. protocol version mismatch)
    /// — straight to Failed, no restart loop.
    Fatal(String),
    /// Dev-mode hot reload: PYLAIR_RESTART_TRIGGER file changed — respawn
    /// immediately, no crash accounting.
    DevRestart,
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
        self.run_loop().await;
        let outcome = self.shutdown_outcome.unwrap_or(ShutdownOutcome::Unknown);
        for ack in self.shutdown_acks.drain(..) {
            let _ = ack.send(outcome);
        }
    }

    /// Wait out a restart backoff, but honour Shutdown immediately and fail
    /// other commands fast (the runtime is down; callers must not hang).
    /// Returns true if shutdown was requested.
    async fn sleep_or_shutdown(&mut self, d: Duration) -> bool {
        let deadline = tokio::time::sleep(d);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                _ = &mut deadline => return false,
                cmd = self.cmd_rx.recv() => match cmd {
                    None => return true,
                    Some(Command::Shutdown { ack }) => {
                        self.shutdown_acks.push(ack);
                        return true;
                    }
                    Some(c) => fail_command_fast(c, "runtime is restarting"),
                },
            }
        }
    }

    async fn run_loop(&mut self) {
        let mut crashes: VecDeque<Instant> = VecDeque::new();
        let mut backoff = self.config.backoff_base;
        loop {
            self.ready_since = None;
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
                ExitCause::DevRestart => {
                    self.set_state(State::Restarting, "source changed");
                    self.generation += 1;
                }
                ExitCause::Fatal(reason) => {
                    self.set_state(State::Failed, reason);
                    if !self.park_failed().await {
                        return;
                    }
                    // operator retried: fresh counters, spawn again
                    crashes.clear();
                    backoff = self.config.backoff_base;
                    self.generation += 1;
                }
                ExitCause::Crashed(reason) => {
                    // Surviving Ready for a whole breaker window counts as
                    // healthy — the crash that follows is a fresh incident,
                    // not evidence of a boot loop.
                    if self
                        .ready_since
                        .is_some_and(|t| t.elapsed() >= self.config.breaker_window)
                    {
                        crashes.clear();
                        backoff = self.config.backoff_base;
                    }
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
                        if !self.park_failed().await {
                            return;
                        }
                        crashes.clear();
                        backoff = self.config.backoff_base;
                        self.generation += 1;
                        continue;
                    }
                    self.set_state(State::Restarting, reason);
                    if self.sleep_or_shutdown(jitter(backoff)).await {
                        self.set_state(State::Stopped, "shutdown");
                        return;
                    }
                    backoff = (backoff * 2).min(self.config.backoff_cap);
                    self.generation += 1;
                }
            }
        }
    }

    /// Failed is a parked state, not an exit — wait for Retry or Shutdown.
    /// Incoming calls are failed fast so callers never hang.
    async fn park_failed(&mut self) -> bool {
        loop {
            match self.cmd_rx.recv().await {
                None => {
                    self.set_state(State::Stopped, "shutdown");
                    return false;
                }
                Some(Command::Shutdown { ack }) => {
                    self.shutdown_acks.push(ack);
                    self.set_state(State::Stopped, "shutdown");
                    return false;
                }
                Some(Command::Retry) => return true,
                Some(c) => fail_command_fast(c, "runtime is stopped"),
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
            // Not a crash — a deployment bug. Retrying can't fix it.
            return ExitCause::Fatal(format!(
                "protocol version mismatch: {:?}",
                hello["protocol"]
            ));
        }
        let _ = self
            .events
            .try_send(RuntimeEvent::Exposes(hello["exposes"].clone()));
        self.ready_since = Some(Instant::now());
        self.set_state(State::Ready, "handshake complete");

        // ---- serving ----
        let mut pending: HashMap<u64, PendingKind> = HashMap::new();
        let mut task_of_call: HashMap<u64, u64> = HashMap::new(); // call_id -> task
                                                                  // Task registry {task -> call_id, owner webview} — the ownership
                                                                  // check for cancel/credit happens here, and the map is per-session
                                                                  // so a stale id from a previous generation resolves to NotFound.
        let mut tasks: HashMap<u64, TaskMeta> = HashMap::new();
        // Recently-finished task ids — lets cancel report Completed
        // instead of NotFound. Bounded; cleared on overflow.
        let mut done_tasks: HashSet<u64> = HashSet::new();
        let mut pongs_outstanding: u32 = 0;
        let mut ping_seq: u64 = 0;
        let mut ping_tick = interval(self.config.ping_interval);
        ping_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        // Dev-mode hot reload: the CLI touches the trigger file when source
        // changes; we poll its mtime (no fs-watch dep). Config wins over env.
        let restart_trigger = self
            .config
            .restart_trigger
            .clone()
            .or_else(|| std::env::var_os("PYLAIR_RESTART_TRIGGER").map(PathBuf::from));
        let trigger_mtime = restart_trigger.as_deref().and_then(mtime);
        let mut trigger_tick = interval(Duration::from_millis(250));

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
                                    PendingKind::Stream(sink) => sink.finish(out),
                                    PendingKind::Cancel { respond } => {
                                        let _ = respond.send(cancel_outcome(&out));
                                    }
                                }
                            }
                            if let Some(task) = task_of_call.remove(&id) {
                                tasks.remove(&task);
                                if done_tasks.len() >= 1024 {
                                    done_tasks.clear();
                                }
                                done_tasks.insert(task);
                            }
                        }
                        Some(Ok(InMsg::Notification { method, params })) => {
                            let p = params.unwrap_or(Value::Null);
                            match method.as_str() {
                                "pong" => {
                                    pongs_outstanding = 0;
                                    // Transport is alive. Is the loop SERVING?
                                    let stale = Duration::from_millis(p["loop_stale_ms"].as_u64().unwrap_or(0));
                                    match classify_pong(stale, self.config.loop_stall_kill) {
                                        PongVerdict::StallKill => {
                                            self.set_state(State::Unresponsive,
                                                format!("event loop blocked for {stale:?}"));
                                            fail_all(&mut pending, proto::RUNTIME_WEDGED,
                                                &format!("the runtime's event loop has been blocked for {stale:?} \
                                                          (blocking code inside an async def?)")).await;
                                            kill_and_reap(&mut io).await;
                                            return ExitCause::Crashed(format!("event loop blocked for {stale:?}"));
                                        }
                                        // Diagnostic only (P1-7): the loop
                                        // cannot serve calls right now, but a
                                        // long stall is not proof of a wedge.
                                        PongVerdict::StallWarn => self.set_state(State::Unresponsive,
                                            format!("event loop blocked for {stale:?} (diagnostic)")),
                                        PongVerdict::Ready => self.set_state(State::Ready, "pong received"),
                                    }
                                }
                                "stream.chunk" => {
                                    let task = p["task"].as_u64().unwrap_or(0);
                                    let seq = p["seq"].as_u64().unwrap_or(0);
                                    let value = p["value"].clone();
                                    let mut abandoned = None;
                                    if let Some(call_id) = tasks.get(&task).map(|m| m.call_id) {
                                        if let Some(PendingKind::Stream(sink)) = pending.get(&call_id) {
                                            if let Err(why) = sink.chunk(seq, value) {
                                                abandoned = Some((call_id, why));
                                            }
                                        }
                                    }
                                    if let Some((call_id, why)) = abandoned {
                                        // The consumer stalled or vanished. End the stream
                                        // with a terminal that cannot be lost, and STOP the
                                        // producer instead of leaving it running unobserved.
                                        let (msg, note) = match why {
                                            ChunkFail::Stalled => ("stream consumer stalled", "stalled"),
                                            ChunkFail::Gone => ("stream consumer went away", "went away"),
                                        };
                                        let _ = self.events.try_send(RuntimeEvent::Stderr(
                                            format!("task {task}: stream consumer {note}; cancelling producer")));
                                        if let Some(PendingKind::Stream(sink)) = pending.remove(&call_id) {
                                            sink.finish(Err(ErrorObj {
                                                code: proto::INTERNAL,
                                                message: msg.into(),
                                                data: None,
                                            }));
                                        }
                                        task_of_call.remove(&call_id);
                                        tasks.remove(&task);
                                        let cid = self.next_id.fetch_add(1, Ordering::Relaxed);
                                        let (ack, _) = oneshot::channel();
                                        if send_ctrl(&io, &proto::request(cid, "cancel",
                                            serde_json::json!({"task": task}))) {
                                            pending.insert(cid, PendingKind::Cancel { respond: ack });
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
                        let outcome = if kill_and_reap(&mut io).await {
                            ShutdownOutcome::ForcedKill
                        } else {
                            ShutdownOutcome::Incomplete
                        };
                        self.shutdown_outcome = Some(outcome);
                        return ExitCause::CleanShutdown;
                    };
                    match cmd {
                        Command::Call { fn_name, input, respond, task_out, owner } => {
                            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
                            let task = self.next_task.fetch_add(1, Ordering::Relaxed);
                            let body = proto::request(id, "call",
                                serde_json::json!({"fn": fn_name, "task": task, "input": input}));
                            if send_frame(&io, &body) {
                                let _ = task_out.send(task);
                                pending.insert(id, PendingKind::Call(respond));
                                task_of_call.insert(id, task);
                                tasks.insert(task, TaskMeta { call_id: id, owner });
                            } else {
                                let _ = respond.send(Err(ErrorObj {
                                    code: proto::RUNTIME_CRASHED,
                                    message: "runtime stdin is gone".into(), data: None }));
                            }
                        }
                        Command::CallStream { fn_name, input, items, task_out, owner } => {
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
                                tasks.insert(task, TaskMeta { call_id: id, owner });
                            } else {
                                items.finish(Err(stopped_err()));
                            }
                        }
                        Command::Cancel { task, respond, caller } => {
                            match tasks.get(&task) {
                                // Unknown to this generation: a completed
                                // tombstone means the cancel raced a finish;
                                // anything else is a stale or foreign id.
                                None => {
                                    let _ = respond.send(if done_tasks.contains(&task) {
                                        CancelOutcome::Completed
                                    } else {
                                        CancelOutcome::NotFound
                                    });
                                }
                                Some(meta) if !owner_allows(&meta.owner, &caller) => {
                                    let _ = respond.send(CancelOutcome::Rejected);
                                }
                                Some(_) => {
                                    let id = self.next_id.fetch_add(1, Ordering::Relaxed);
                                    let body = proto::request(id, "cancel",
                                        serde_json::json!({"task": task}));
                                    // Control lane: a data flood cannot starve
                                    // this, and a failed try_send is a loud
                                    // outcome — never a silent drop.
                                    if send_ctrl(&io, &body) {
                                        pending.insert(id, PendingKind::Cancel { respond });
                                    } else {
                                        let _ = respond.send(CancelOutcome::DeliveryFailed);
                                    }
                                }
                            }
                        }
                        Command::GrantCredit { task, n, respond, caller } => {
                            match tasks.get(&task) {
                                None => {
                                    let _ = respond.send(Err(ErrorObj {
                                        code: proto::TASK_NOT_FOUND,
                                        message: if done_tasks.contains(&task) {
                                            format!("task {task} already finished")
                                        } else {
                                            format!("no such task in this generation: {task}")
                                        },
                                        data: None,
                                    }));
                                }
                                Some(meta) if !owner_allows(&meta.owner, &caller) => {
                                    let _ = respond.send(Err(ErrorObj {
                                        code: proto::INVALID_REQUEST,
                                        message: format!("task {task} belongs to a different webview"),
                                        data: None,
                                    }));
                                }
                                Some(_) => {
                                    let body = proto::notification("stream.credit",
                                        serde_json::json!({"task": task, "n": n}));
                                    let _ = respond.send(if send_ctrl(&io, &body) {
                                        Ok(())
                                    } else {
                                        Err(ErrorObj {
                                            code: proto::RUNTIME_UNAVAILABLE,
                                            message: "control queue full — credit not delivered".into(),
                                            data: None,
                                        })
                                    });
                                }
                            }
                        }
                        Command::CancelOwned { owner } => {
                            // A window was destroyed: cancel every task it
                            // owned. Supervisor-initiated, so the owner check
                            // does not apply; delivery failures only lose a
                            // cancel the dead window could never observe.
                            for (&task, meta) in tasks.iter() {
                                if meta.owner.as_deref() == Some(owner.as_str()) {
                                    let id = self.next_id.fetch_add(1, Ordering::Relaxed);
                                    let body = proto::request(id, "cancel",
                                        serde_json::json!({"task": task}));
                                    let (ack, _) = oneshot::channel();
                                    if send_ctrl(&io, &body) {
                                        pending.insert(id, PendingKind::Cancel { respond: ack });
                                    }
                                }
                            }
                        }
                        Command::Retry => {
                            // Only meaningful from Failed — already serving.
                        }
                        Command::Shutdown { ack } => {
                            self.shutdown_acks.push(ack);
                            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
                            let body = proto::request(id, "shutdown", Value::Null);
                            send_ctrl(&io, &body);
                            // Graceful exit within the grace window consumes
                            // `exited`; on timeout it is still pollable, so
                            // kill the tree and wait for the reap. Report
                            // what actually happened (RC.6C).
                            let outcome = if timeout(self.config.shutdown_grace, &mut io.exited).await.is_ok() {
                                ShutdownOutcome::Terminated
                            } else if kill_and_reap(&mut io).await {
                                ShutdownOutcome::ForcedKill
                            } else {
                                ShutdownOutcome::Incomplete
                            };
                            self.shutdown_outcome = Some(outcome);
                            fail_all(&mut pending, proto::RUNTIME_UNAVAILABLE, "shutdown").await;
                            return ExitCause::CleanShutdown;
                        }
                    }
                }
                // --- liveness ping ---
                _ = ping_tick.tick() => {
                    if pongs_outstanding >= self.config.missed_pongs_to_kill {
                        self.set_state(State::Unresponsive,
                            format!("{} missed pongs — runtime wedged", pongs_outstanding));
                        fail_all(&mut pending, proto::RUNTIME_WEDGED,
                            "runtime stopped responding (possible native-code wedge)").await;
                        kill_and_reap(&mut io).await;
                        return ExitCause::Crashed(format!(
                            "wedged: {} missed pongs", pongs_outstanding));
                    }
                    if pongs_outstanding > 0 {
                        // A ping went a full interval unanswered — a real miss.
                        self.set_state(State::Unresponsive,
                            format!("missed {} pong(s)", pongs_outstanding));
                    }
                    pongs_outstanding += 1;
                    ping_seq += 1;
                    let body = proto::request(self.next_id.fetch_add(1, Ordering::Relaxed),
                        "ping", serde_json::json!({"seq": ping_seq}));
                    send_frame(&io, &body);
                }
                // --- dev restart trigger ---
                _ = trigger_tick.tick(), if restart_trigger.is_some() => {
                    let m = restart_trigger.as_deref().and_then(mtime);
                    if m != trigger_mtime {
                        let _ = self.events.try_send(RuntimeEvent::Stderr(
                            "pylair: source changed — restarting python runtime".into()));
                        fail_all(&mut pending, proto::RUNTIME_RESTARTED,
                            "python restarted (source change)").await;
                        kill_and_reap(&mut io).await;
                        return ExitCause::DevRestart;
                    }
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

/// Answer a command immediately when there is no runtime to run it.
fn fail_command_fast(c: Command, why: &str) {
    let err = || ErrorObj {
        code: proto::RUNTIME_UNAVAILABLE,
        message: why.into(),
        data: None,
    };
    match c {
        Command::Call { respond, .. } => {
            let _ = respond.send(Err(err()));
        }
        Command::CallStream { items, .. } => items.finish(Err(err())),
        Command::Cancel { respond, .. } => {
            let _ = respond.send(CancelOutcome::NotFound);
        }
        Command::GrantCredit { respond, .. } => {
            let _ = respond.send(Err(err()));
        }
        Command::CancelOwned { .. } | Command::Retry => {}
        Command::Shutdown { ack } => {
            let _ = ack.send(ShutdownOutcome::Unknown);
        }
    }
}

fn mtime(p: &std::path::Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

fn send_frame(io: &ChildIo, body: &Value) -> bool {
    match serde_json::to_vec(body) {
        Ok(bytes) => io.frame_out.try_send(bytes).is_ok(),
        Err(_) => false,
    }
}

/// Control lane: cancel / credit / shutdown frames the writer drains before
/// data frames. Bounded — a failed try_send is a visible failure, not a drop.
fn send_ctrl(io: &ChildIo, body: &Value) -> bool {
    match serde_json::to_vec(body) {
        Ok(bytes) => io.ctrl_out.try_send(bytes).is_ok(),
        Err(_) => false,
    }
}

/// May `caller` control a task recorded with `owner`? Unowned tasks
/// (supervisor-direct callers, tests) are open to anyone; owned tasks only
/// to the exact webview label Tauri reported for the invoking window.
fn owner_allows(owner: &Option<String>, caller: &Option<String>) -> bool {
    match owner {
        None => true,
        Some(o) => caller.as_deref() == Some(o.as_str()),
    }
}

/// Map the runtime's cancel ack — `{"cancelled": bool, "status"?, "reason"?}`
/// — to a distinct outcome. An error reply means the cancel itself failed.
fn cancel_outcome(out: &Result<Value, ErrorObj>) -> CancelOutcome {
    match out {
        Ok(v) if v["cancelled"].as_bool().unwrap_or(false) => match v["status"].as_str() {
            Some("terminated") => CancelOutcome::Terminated,
            Some("may_continue") => CancelOutcome::RequestedMayContinue,
            Some("termination_in_progress") => CancelOutcome::TerminationUncertain,
            _ => CancelOutcome::Cancelled,
        },
        // task_not_found from the runtime = it finished before we asked.
        Ok(_) => CancelOutcome::Completed,
        Err(_) => CancelOutcome::TerminationUncertain,
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
            PendingKind::Stream(sink) => sink.finish(Err(err)),
            PendingKind::Cancel { respond } => {
                let _ = respond.send(CancelOutcome::TerminationUncertain);
            }
        }
    }
}

/// Signal the kill, then wait for the monitor to reap — the next spawn must
/// not overlap a still-alive previous generation. Returns whether the exit
/// was confirmed inside the wait window.
async fn kill_and_reap(io: &mut ChildIo) -> bool {
    let _ = io.kill.try_send(());
    timeout(Duration::from_secs(5), &mut io.exited)
        .await
        .is_ok()
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

/// Linux: spawn the child FROM a dedicated thread that then parks for the
/// process's lifetime. PR_SET_PDEATHSIG is bound to the *thread* that
/// forked the child — not the process — so spawning on a transient
/// executor thread (e.g. a spawn_blocking worker at its ~10s keepalive)
/// would SIGKILL a perfectly healthy runtime. The anchor can only die
/// with the whole process, so the signal means exactly "the host is gone".
///
/// The fork itself must happen on the anchor thread, so the runtime
/// handle is carried in and entered there — `Command::spawn` is a plain
/// synchronous call that only needs the context to register the child.
#[cfg(target_os = "linux")]
fn spawn_via_pdeathsig_anchor(spec: &SpawnSpec) -> io::Result<Child> {
    use std::os::unix::process::CommandExt;
    let rt = tokio::runtime::Handle::try_current()
        .map_err(|e| io::Error::other(format!("no runtime for spawn: {e}")))?;
    let mut cmd = TokioCommand::new(dunce::simplified(&spec.program));
    cmd.args(&spec.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .process_group(0);
    if let Some(cwd) = &spec.cwd {
        cmd.current_dir(cwd);
    }
    for k in &spec.env_remove {
        cmd.env_remove(k);
    }
    cmd.envs(spec.env.iter().cloned());
    unsafe {
        cmd.as_std_mut().pre_exec(|| {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong) != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let (tx, rx) = std::sync::mpsc::channel::<io::Result<Child>>();
    std::thread::Builder::new()
        .name("pylair-pdeath-anchor".into())
        .spawn(move || {
            // Enter the runtime so the child's registration resolves, then
            // fork HERE — the JoinHandle detaches on return and this thread
            // lives until the process does.
            let _guard = rt.enter();
            let _ = tx.send(cmd.spawn());
            loop {
                std::thread::park();
            }
        })?;
    rx.recv()
        .map_err(|_| io::Error::other("pdeathsig anchor died before spawn"))?
}

async fn spawn_child(
    spec: &SpawnSpec,
    config: &SupervisorConfig,
    events: &mpsc::Sender<RuntimeEvent>,
) -> io::Result<ChildIo> {
    // Simplify a canonicalized (Windows \\?\-prefixed) program path so the
    // child sees a plain sys.executable — dev-mode callers may pass one too.
    // On Linux the spawn happens on a dedicated anchor thread instead —
    // see spawn_via_pdeathsig_anchor.
    #[cfg(not(target_os = "linux"))]
    let mut child = {
        let mut cmd = TokioCommand::new(dunce::simplified(&spec.program));
        cmd.args(&spec.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(cwd) = &spec.cwd {
            cmd.current_dir(cwd);
        }
        for k in &spec.env_remove {
            cmd.env_remove(k);
        }
        cmd.envs(spec.env.iter().cloned());
        #[cfg(unix)]
        {
            // Own process group so killpg can reach grandchildren (§10).
            cmd.process_group(0);
        }
        cmd.spawn()?
    };
    #[cfg(target_os = "linux")]
    let mut child = spawn_via_pdeathsig_anchor(spec)?;
    #[cfg(windows)]
    let pid = child.id();
    let stdin = child.stdin.take().expect("piped");
    let stdout = child.stdout.take().expect("piped");
    let stderr = child.stderr.take().expect("piped");

    // Job Object handle must outlive the child — held by the monitor task.
    #[cfg(windows)]
    let job = assign_to_job_object(pid);
    #[cfg(windows)]
    if job.is_none() {
        let _ = events.try_send(RuntimeEvent::Stderr(
            "pylair: could not place the runtime in a Job Object; falling back to \
             taskkill /T for cleanup (descendants may survive a hard host exit)"
                .into(),
        ));
    }
    #[cfg(windows)]
    let tree_pid = pid;

    // monitor: owns the child, awaits exit or kill signal
    let (exit_tx, exit_rx) = oneshot::channel();
    let (kill_tx, kill_rx) = mpsc::channel::<()>(1);
    #[cfg(windows)]
    tokio::spawn(monitor_child(child, kill_rx, exit_tx, job, tree_pid));
    #[cfg(not(windows))]
    tokio::spawn(monitor_child(child, kill_rx, exit_tx));

    // writer: frames -> child stdin. Two lanes: control (cancel/credit/
    // shutdown, small and always drained first) and data (calls, initial
    // credit). A stream flood can fill the data lane; it cannot starve or
    // silently drop a control frame (RC.6B).
    let (frame_tx, mut frame_rx) = mpsc::channel::<Vec<u8>>(64);
    let (ctrl_tx, mut ctrl_rx) = mpsc::channel::<Vec<u8>>(16);
    tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        let mut w = stdin;
        loop {
            let body: Vec<u8> = tokio::select! {
                biased;
                // Ctrl always wins when both lanes have a frame; a closed
                // ctrl channel falls through to the data lane.
                c = ctrl_rx.recv() => match c {
                    Some(b) => b,
                    None => match frame_rx.recv().await {
                        Some(b) => b,
                        None => return,
                    },
                },
                d = frame_rx.recv() => match d {
                    Some(b) => b,
                    None => match ctrl_rx.recv().await {
                        Some(b) => b,
                        None => return,
                    },
                },
            };
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
        ctrl_out: ctrl_tx,
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
    #[cfg(unix)]
    let pgid = child.id();
    tokio::select! {
        status = child.wait() => {
            // The leader exiting on its own must not leave group members
            // (workers, user subprocesses) running with no supervisor.
            #[cfg(unix)]
            if let Some(pgid) = pgid {
                reap_group(pgid);
            }
            let _ = exit_tx.send(status);
        }
        _ = kill_rx.recv() => {
            kill_tree(&mut child).await;
            let status = child.wait().await;
            let _ = exit_tx.send(status);
        }
    }
}

/// SIGKILL every remaining member of the process group `pgid` we created.
/// ESRCH (already empty) and EPERM (group id recycled by an unrelated
/// process) are both "nothing of ours left to kill".
#[cfg(unix)]
fn reap_group(pgid: u32) {
    unsafe {
        libc::kill(-(pgid as i32), libc::SIGKILL);
    }
}

#[cfg(windows)]
async fn monitor_child(
    mut child: Child,
    mut kill_rx: mpsc::Receiver<()>,
    exit_tx: oneshot::Sender<io::Result<std::process::ExitStatus>>,
    // Holding this keeps the Job Object alive; it closes when the monitor
    // task ends (after the child exits), which also kills any survivors.
    job: Option<JobHandle>,
    tree_pid: Option<u32>,
) {
    tokio::select! {
        status = child.wait() => { let _ = exit_tx.send(status); }
        _ = kill_rx.recv() => {
            if job.is_none() {
                // No Job Object: taskkill /T is the only way to reach the tree.
                if let Some(pid) = tree_pid {
                    let _ = tokio::process::Command::new("taskkill")
                        .args(["/PID", &pid.to_string(), "/T", "/F"])
                        .output()
                        .await;
                }
            }
            let _ = child.start_kill(); // Job Object also reaps on handle close
            let status = child.wait().await;
            let _ = exit_tx.send(status);
        }
    }
}

/// Kill the child process tree: killpg on unix (own process group, protocol
/// §10); other targets fall back to plain child kill. On Windows the Job
/// Object reaps the tree when its handle closes (see monitor_child).
#[cfg(not(windows))]
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

/// windows-sys HANDLE is *mut c_void, which is !Send — wrap it so it can be
/// held by the monitor task (keeps the Job Object alive for the child's life).
#[cfg(windows)]
struct JobHandle(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
unsafe impl Send for JobHandle {}

#[cfg(windows)]
impl Drop for JobHandle {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

#[cfg(windows)]
fn assign_to_job_object(pid: Option<u32>) -> Option<JobHandle> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::JobObjects::*;
    use windows_sys::Win32::System::Threading::*;

    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
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
        if proc.is_null() || AssignProcessToJobObject(job, proc) == 0 {
            if !proc.is_null() {
                CloseHandle(proc);
            }
            CloseHandle(job);
            return None;
        }
        CloseHandle(proc);
        Some(JobHandle(job))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pong_classification_boundaries() {
        use PongVerdict::*;
        let kill = Some(Duration::from_secs(300));
        // Below the warn threshold: Ready even with a kill configured.
        assert_eq!(classify_pong(Duration::ZERO, kill), Ready);
        assert_eq!(
            classify_pong(LOOP_STALL_WARN - Duration::from_millis(1), kill),
            Ready
        );
        // Warn band: from threshold up to (but not including) kill.
        assert_eq!(classify_pong(LOOP_STALL_WARN, kill), StallWarn);
        assert_eq!(classify_pong(Duration::from_secs(299), kill), StallWarn);
        // At/over the configured kill: StallKill.
        assert_eq!(classify_pong(Duration::from_secs(300), kill), StallKill);
        // No kill configured: any stall is only a diagnostic, forever.
        assert_eq!(classify_pong(Duration::from_secs(3600), None), StallWarn);
        assert_eq!(classify_pong(Duration::MAX, None), StallWarn);
        // A kill configured below the warn threshold still wins (config is king).
        assert_eq!(
            classify_pong(Duration::from_millis(500), Some(Duration::from_millis(500))),
            StallKill
        );
    }
}
