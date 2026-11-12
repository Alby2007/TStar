//! Step 0 reproduction tests for the RC.6 hardening batch. Each test
//! asserts the DESIRED invariant — a failure means the claim reproduces
//! at HEAD. Do not weaken assertions to make them pass; fix the code.
//!
//!   1. non_owner_* — a webview that does not own a task can cancel /
//!      grant-credit on its id and the request is honoured.
//!   2. cancel_not_dropped_when_frame_queue_full — a cancel can be
//!      dropped silently when the host->runtime frame queue is full.
//!   3. host_sigkill_* — an abrupt host kill leaves the runtime (or a
//!      grandchild worker) running.

use serde_json::json;
use std::path::PathBuf;
use std::time::Duration;
use tauri_plugin_pylair::protocol as proto;
use tauri_plugin_pylair::supervisor::{
    spawn, Handle, RuntimeEvent, SpawnSpec, State, SupervisorConfig,
};
use tokio::sync::mpsc;
use tokio::time::{sleep, timeout};

const T: Duration = Duration::from_secs(15);

// ---- shared harness (same conventions as supervisor_faults.rs) ----

fn spawn_h(
    spec: SpawnSpec,
    config: SupervisorConfig,
    events: mpsc::Sender<RuntimeEvent>,
) -> Handle {
    let (h, actor) = spawn(spec, config, events);
    tokio::spawn(actor);
    h
}

fn python() -> PathBuf {
    if let Ok(p) = std::env::var("PYLAIR_TEST_PYTHON") {
        return p.into();
    }
    for cand in ["python3", "python"] {
        let ok = std::process::Command::new(cand)
            .arg("-c")
            .arg("import sys;sys.exit(0 if sys.version_info>=(3,12) else 1)")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if ok {
            return cand.into();
        }
    }
    panic!("no python >=3.12 found for supervisor tests");
}

fn real_runtime_spec() -> SpawnSpec {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    SpawnSpec {
        program: python(),
        args: vec!["-m".into(), "pylair".into(), "testapp".into()],
        env: vec![(
            "PYTHONPATH".into(),
            root.join("python/pylair/src")
                .to_string_lossy()
                .into_owned(),
        )],
        cwd: Some(root.join("python/pylair/tests")),
        ..Default::default()
    }
}

fn spec(script: &str) -> SpawnSpec {
    SpawnSpec {
        program: python(),
        args: vec!["-c".into(), script.into()],
        env: vec![],
        cwd: None,
        ..Default::default()
    }
}

fn fast_config() -> SupervisorConfig {
    SupervisorConfig {
        hello_timeout: Duration::from_secs(2),
        ping_interval: Duration::from_millis(150),
        missed_pongs_to_kill: 2,
        backoff_base: Duration::from_millis(20),
        backoff_cap: Duration::from_millis(80),
        breaker_crashes: 2,
        breaker_window: Duration::from_secs(60),
        shutdown_grace: Duration::from_secs(2),
        initial_stream_credit: 8,
        stream_buffer: 256,
        loop_stall_kill: None,
        cancel_timeout: Duration::from_secs(5),
        max_frame: proto::max_frame(),
        restart_trigger: None,
    }
}

async fn wait_state(h: &Handle, want: State) {
    let mut rx = h.subscribe_state();
    timeout(T, async {
        while *rx.borrow() != want {
            rx.changed().await.expect("state channel closed");
        }
    })
    .await
    .unwrap_or_else(|_| panic!("state never reached {want:?} (at {:?})", h.state()));
}

/// Well-behaved fake runtime: hello, echo, pong, exit on shutdown/EOF.
const FAKE_READY: &str = r#"
import sys, struct, json
def w(o):
    b = json.dumps(o).encode()
    sys.stdout.buffer.write(struct.pack(">I", len(b)) + b); sys.stdout.buffer.flush()
w({"jsonrpc":"2.0","method":"hello","params":{"protocol":1,"exposes":[{"name":"echo","kind":"sync"}]}})
while True:
    h = sys.stdin.buffer.read(4)
    if len(h) < 4: break
    n, = struct.unpack(">I", h)
    m = json.loads(sys.stdin.buffer.read(n))
    if m["method"] == "ping":
        w({"jsonrpc":"2.0","method":"pong","params":{"seq": m["params"]["seq"]}})
    elif m["method"] == "shutdown":
        w({"jsonrpc":"2.0","id": m["id"],"result": None}); break
    elif "id" in m:
        w({"jsonrpc":"2.0","id": m["id"],"result": m["params"].get("input")})
"#;

/// Hello, one pong, then wedges forever without ever reading stdin —
/// stands in for a runtime whose loop (or reader) is dead: nothing can
/// observe stdin EOF, so no drain ever runs.
const FAKE_WEDGED: &str = r#"
import sys, struct, json
b = json.dumps({"jsonrpc":"2.0","method":"hello","params":{"protocol":1,"exposes":[{"name":"echo","kind":"sync"}]}}).encode()
sys.stdout.buffer.write(struct.pack(">I", len(b)) + b); sys.stdout.buffer.flush()
import time
time.sleep(600)
"#;

/// Reads stdin (so it dies on EOF like a healthy runtime) but first
/// spawns a sleeper grandchild it never reaps — stands in for a runtime
/// that exits without killing an in-flight isolated worker.
#[cfg(unix)]
const FAKE_PARENT_UNREAPED_CHILD: &str = r#"
import sys, struct, json, subprocess
def w(o):
    b = json.dumps(o).encode()
    sys.stdout.buffer.write(struct.pack(">I", len(b)) + b); sys.stdout.buffer.flush()
subprocess.Popen(["sleep", "600"])
w({"jsonrpc":"2.0","method":"hello","params":{"protocol":1,"exposes":[]}})
while True:
    h = sys.stdin.buffer.read(4)
    if len(h) < 4: break
    n, = struct.unpack(">I", h)
    m = json.loads(sys.stdin.buffer.read(n))
    if m["method"] == "shutdown":
        w({"jsonrpc":"2.0","id": m["id"],"result": None}); break
"#;

// ---- claim 1: task ownership ----

/// Two cloned handles are exactly what two webviews hold: the commands
/// in lib.rs forward only `task`, so Handle clones are indistinguishable
/// callers. Desired invariant: a caller that did not start a task must
/// not be able to cancel it. HEAD has no ownership check at any layer —
/// this fails while the gap exists.
#[tokio::test]
async fn non_owner_cannot_cancel_another_windows_task() {
    let (ev, _rx) = mpsc::channel::<RuntimeEvent>(64);
    let h = spawn_h(real_runtime_spec(), fast_config(), ev);
    wait_state(&h, State::Ready).await;

    let h_window_a = h.clone();
    let h_window_b = h.clone();

    let ct = h_window_a.call_task("sleep_forever", json!({})).unwrap();
    let task = timeout(T, ct.task)
        .await
        .expect("task id timed out")
        .unwrap();

    let res = timeout(T, h_window_b.cancel(task))
        .await
        .expect("cancel timed out")
        .unwrap();
    assert!(
        !res,
        "window B cancelled window A's task — no ownership check on python_cancel"
    );

    // The task must still be running (owner's work untouched).
    let still_running = timeout(Duration::from_secs(2), ct.result).await.is_err();
    assert!(still_running, "window B's cancel terminated A's task");
    h.shutdown().await;
}

#[tokio::test]
async fn non_owner_cannot_grant_credit_on_another_windows_stream() {
    let (ev, _rx) = mpsc::channel::<RuntimeEvent>(64);
    let mut cfg = fast_config();
    cfg.initial_stream_credit = 2; // stream blocks quickly without credit
    let h = spawn_h(real_runtime_spec(), cfg, ev);
    wait_state(&h, State::Ready).await;

    let h_window_b = h.clone();
    let (_task, mut items) = timeout(T, h.call_stream("counter", json!({"n": 1_000})))
        .await
        .expect("call_stream timed out")
        .unwrap();
    let task = _task;

    let res = timeout(T, h_window_b.grant_credit(task, 10_000)).await;
    assert!(
        res.is_err(),
        "window B granted credit on window A's stream — no ownership check on python_grant_credit"
    );

    // And B's credit must not have advanced A's stream.
    let mut seen = 0usize;
    while let Ok(Some(item)) = timeout(Duration::from_secs(2), items.recv()).await {
        if matches!(
            item,
            tauri_plugin_pylair::supervisor::StreamItem::Chunk { .. }
        ) {
            seen += 1;
        } else {
            break;
        }
    }
    assert!(
        seen <= 2,
        "window B's credit produced {seen} chunks on A's stream"
    );
    h.shutdown().await;
}

// ---- claim 2: reliable control delivery ----

/// frame_out is a bounded (64) mpsc feeding the child's stdin; send_frame
/// uses try_send. Fill the pipe by pointing it at a runtime that never
/// reads stdin: once the writer blocks on a full pipe, the 64-slot queue
/// stays full and subsequent try_sends fail. Desired invariant: a cancel
/// that could not be delivered must surface as an error (or the control
/// path must be unstarvable) — never an ambiguous `Ok(false)`. GrantCredit
/// must surface failure too; today its send result is discarded entirely.
#[tokio::test]
async fn cancel_not_dropped_when_frame_queue_full() {
    let (ev, _rx) = mpsc::channel::<RuntimeEvent>(64);
    let mut cfg = fast_config();
    // Keep the wedged runtime alive: it can't pong, so the liveness
    // killer must not fire during the flood.
    cfg.missed_pongs_to_kill = 1_000_000;
    let h = spawn_h(spec(FAKE_WEDGED), cfg, ev);
    wait_state(&h, State::Ready).await;

    // Flood: each Call becomes one frame in frame_out. The writer can
    // only drain ~pipe-capacity into a dead reader, so the 64-slot queue
    // fills; Call failures then report RUNTIME_CRASHED loudly — that is
    // our signal the queue is full.
    let big = "x".repeat(16 * 1024);
    let mut queue_full = false;
    for _ in 0..400 {
        match h.call_task("echo", json!({"input": big})) {
            Ok(ct) => {
                if let Ok(Ok(Err(e))) = timeout(T, ct.result).await {
                    if e.code == proto::RUNTIME_CRASHED {
                        queue_full = true;
                        break;
                    }
                }
            }
            Err(_) => {
                queue_full = true;
                break;
            }
        }
    }
    assert!(queue_full, "never managed to fill the frame queue");

    // Now the control-plane assertion: a cancel issued against a full
    // data queue must not be silently swallowed.
    let res = timeout(T, h.cancel(12345))
        .await
        .expect("cancel timed out")
        .unwrap();
    assert!(
        res,
        "cancel returned Ok(false) while the frame queue was full — \
         the frame was dropped and the caller cannot tell it apart from \
         'task already finished'"
    );

    // grant_credit has it worse: its send result is discarded outright.
    let res = timeout(T, h.grant_credit(12345, 8))
        .await
        .expect("grant_credit timed out");
    assert!(
        res.is_err(),
        "grant_credit returned Ok(()) for a frame that was never delivered"
    );
    h.shutdown().await;
}

// ---- claim 3: parent-death cleanup ----

/// Re-exec harness: the controller spawns this same test binary filtered
/// to this test with PYLAIR_ORPHAN_HOST set; that child plays "the app"
/// and is killed with SIGKILL so no Rust Drop ever runs. The supervisor's
/// children are then surveyed via pgrep.
#[cfg(unix)]
fn children_of(pid: u32) -> Vec<u32> {
    let out = std::process::Command::new("pgrep")
        .args(["-P", &pid.to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect()
}

#[cfg(unix)]
fn alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Kill surveyed pids on drop so a failed assertion doesn't leak orphans.
#[cfg(unix)]
struct Reap(Vec<u32>);
#[cfg(unix)]
impl Drop for Reap {
    fn drop(&mut self) {
        for p in &self.0 {
            let _ = std::process::Command::new("kill")
                .args(["-9", &p.to_string()])
                .output();
        }
    }
}

/// Desired invariant: after an abrupt host death, the runtime's process
/// is gone within a bound (stdin-EOF drain is the current fallback —
/// it requires a runtime whose reader and loop are alive). FAKE_WEDGED
/// never observes EOF: if the claim reproduces, the child survives.
#[cfg(unix)]
#[tokio::test]
async fn host_sigkill_does_not_orphan_wedged_runtime() {
    if std::env::var("PYLAIR_ORPHAN_HOST").is_ok() {
        // Host role: spawn the wedged runtime, then live forever until
        // killed. The wedged fake can't pong — disable the liveness kill
        // so the child outlives the host long enough to prove orphaning.
        let (ev, _rx) = mpsc::channel::<RuntimeEvent>(64);
        let mut cfg = fast_config();
        cfg.missed_pongs_to_kill = 1_000_000;
        let _h = spawn_h(spec(FAKE_WEDGED), cfg, ev);
        loop {
            sleep(Duration::from_secs(60)).await;
        }
    }

    let me = std::env::current_exe().unwrap();
    let mut host = std::process::Command::new(me)
        .args([
            "host_sigkill_does_not_orphan_wedged_runtime",
            "--exact",
            "--nocapture",
        ])
        .env("PYLAIR_ORPHAN_HOST", "1")
        .spawn()
        .expect("spawn host");
    let host_pid = host.id();

    // Wait for the runtime child to exist (poll; spawn takes a moment).
    let mut kids = Vec::new();
    for _ in 0..100 {
        kids = children_of(host_pid);
        if !kids.is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _reap = Reap(kids.clone());
    assert!(!kids.is_empty(), "host never spawned a runtime child");

    // SIGKILL the "app" — no Drop, no kill_on_drop, no graceful path.
    std::process::Command::new("kill")
        .args(["-9", &host_pid.to_string()])
        .output()
        .unwrap();
    let _ = host.wait();

    // The runtime must die within a bound (drain grace is ~3s).
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while kids.iter().any(|p| alive(*p)) {
        assert!(
            std::time::Instant::now() < deadline,
            "runtime children {kids:?} survived the host's SIGKILL — orphan risk"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Same assertion for a HEALTHY runtime — documents the boundary where
/// the current fallback works (reader alive -> EOF -> exit).
#[cfg(unix)]
#[tokio::test]
async fn host_sigkill_healthy_runtime_exits_on_eof() {
    if std::env::var("PYLAIR_ORPHAN_HOST").is_ok() {
        let (ev, _rx) = mpsc::channel::<RuntimeEvent>(64);
        let _h = spawn_h(spec(FAKE_READY), fast_config(), ev);
        loop {
            sleep(Duration::from_secs(60)).await;
        }
    }

    let me = std::env::current_exe().unwrap();
    let mut host = std::process::Command::new(me)
        .args([
            "host_sigkill_healthy_runtime_exits_on_eof",
            "--exact",
            "--nocapture",
        ])
        .env("PYLAIR_ORPHAN_HOST", "1")
        .spawn()
        .expect("spawn host");
    let host_pid = host.id();

    let mut kids = Vec::new();
    for _ in 0..100 {
        kids = children_of(host_pid);
        if !kids.is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _reap = Reap(kids.clone());
    assert!(!kids.is_empty(), "host never spawned a runtime child");

    std::process::Command::new("kill")
        .args(["-9", &host_pid.to_string()])
        .output()
        .unwrap();
    let _ = host.wait();

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while kids.iter().any(|p| alive(*p)) {
        assert!(
            std::time::Instant::now() < deadline,
            "healthy runtime children {kids:?} survived the host's SIGKILL"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Grandchild case: the "runtime" exits on EOF but leaves a sleeper
/// child unreaped — the shape of an isolated worker whose runtime-side
/// reap never ran (wedged drain).
#[cfg(unix)]
#[tokio::test]
async fn host_sigkill_does_not_orphan_grandchild_worker() {
    if std::env::var("PYLAIR_ORPHAN_HOST").is_ok() {
        let (ev, _rx) = mpsc::channel::<RuntimeEvent>(64);
        let _h = spawn_h(spec(FAKE_PARENT_UNREAPED_CHILD), fast_config(), ev);
        loop {
            sleep(Duration::from_secs(60)).await;
        }
    }

    let me = std::env::current_exe().unwrap();
    let mut host = std::process::Command::new(me)
        .args([
            "host_sigkill_does_not_orphan_grandchild_worker",
            "--exact",
            "--nocapture",
        ])
        .env("PYLAIR_ORPHAN_HOST", "1")
        .spawn()
        .expect("spawn host");
    let host_pid = host.id();

    // Find the runtime child = the child of the host that itself has a
    // child (the "isolated worker"). Polling only "a child of host" can
    // catch the transient python() version probe instead.
    let (mut runtime_pid, mut worker_pid) = (0u32, 0u32);
    for _ in 0..100 {
        for cand in children_of(host_pid) {
            if let Some(w) = children_of(cand).first() {
                runtime_pid = cand;
                worker_pid = *w;
                break;
            }
        }
        if worker_pid != 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(runtime_pid != 0, "host never spawned a runtime child");
    assert!(worker_pid != 0, "runtime never spawned a worker child");

    let _reap = Reap(vec![runtime_pid, worker_pid]);
    std::process::Command::new("kill")
        .args(["-9", &host_pid.to_string()])
        .output()
        .unwrap();
    let _ = host.wait();

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while alive(runtime_pid) || alive(worker_pid) {
        assert!(
            std::time::Instant::now() < deadline,
            "worker {worker_pid} survived after runtime {runtime_pid} exited — \
             an unreaped isolated worker is an orphan"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

// The Windows variant of the SIGKILL tests lands with RC.6C — the unix
// arms cover the mechanism (no PDEATHSIG; drain requires a live loop).
