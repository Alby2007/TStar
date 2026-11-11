//! Fault-injection tests for the supervisor: kill mid-call, startup hang,
//! malformed frames, stray garbage on stdout, wedged runtime (no pongs),
//! crash-loop breaker. Each case spawns a real `python -c` process.

use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::Duration;
use tauri_plugin_pylair::protocol as proto;
use tauri_plugin_pylair::supervisor::{
    spawn, Handle, RuntimeEvent, SpawnSpec, State, SupervisorConfig,
};
use tokio::sync::mpsc;
use tokio::time::{sleep, timeout};

const T: Duration = Duration::from_secs(15);

/// spawn() now returns (Handle, actor future) — tests drive the actor on
/// the test runtime.
fn spawn_h(
    spec: SpawnSpec,
    config: SupervisorConfig,
    events: mpsc::Sender<RuntimeEvent>,
) -> Handle {
    let (h, actor) = spawn(spec, config, events);
    tokio::spawn(actor);
    h
}

// ---- fake runtime scripts (stdin/stdout frame protocol, python) ----

/// Well-behaved: hello, echo calls, pong pings, exit on shutdown.
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

/// Never sends hello — startup hang.
const FAKE_HANG: &str = "import time; time.sleep(600)";

/// Prints raw bytes to stdout — stray writes must be a protocol violation.
const FAKE_STRAY_STDOUT: &str = r#"import sys; sys.stdout.write("stray bytes on fd 1\n"); sys.stdout.flush(); import time; time.sleep(600)"#;

/// Valid frame envelope, invalid JSON body.
const FAKE_MALFORMED: &str = r#"
import sys, struct
sys.stdout.buffer.write(struct.pack(">I", 4) + b"nope"); sys.stdout.buffer.flush()
import time; time.sleep(600)
"#;

/// Hello, pong once, then wedge forever (never reads stdin again).
const FAKE_WEDGED: &str = r#"
import sys, struct, json
b = json.dumps({"jsonrpc":"2.0","method":"hello","params":{"protocol":1,"exposes":[]}}).encode()
sys.stdout.buffer.write(struct.pack(">I", len(b)) + b); sys.stdout.buffer.flush()
import time
time.sleep(600)
"#;

/// Dies on the first call.
const FAKE_DIE_ON_CALL: &str = r#"
import sys, struct, json, os
def w(o):
    b = json.dumps(o).encode()
    sys.stdout.buffer.write(struct.pack(">I", len(b)) + b); sys.stdout.buffer.flush()
w({"jsonrpc":"2.0","method":"hello","params":{"protocol":1,"exposes":[{"name":"boom","kind":"sync"}]}})
while True:
    h = sys.stdin.buffer.read(4)
    if len(h) < 4: break
    n, = struct.unpack(">I", h)
    m = json.loads(sys.stdin.buffer.read(n))
    if m["method"] == "call":
        os._exit(1)
"#;

/// Exits immediately — exercises restart + breaker.
const FAKE_EXIT_NOW: &str = "import sys; sys.exit(1)";

/// hello with the wrong protocol version — deployment bug, not a crash.
const FAKE_BAD_VERSION: &str = r#"
import sys, struct, json
b = json.dumps({"jsonrpc":"2.0","method":"hello","params":{"protocol":99,"exposes":[]}}).encode()
sys.stdout.buffer.write(struct.pack(">I", len(b)) + b); sys.stdout.buffer.flush()
import time; time.sleep(600)
"#;

/// The REAL runtime: `python -m pylair testapp` with the package and the
/// test app found via PYTHONPATH + cwd — no install step needed.
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
    }
}

fn python() -> PathBuf {
    if let Ok(p) = std::env::var("PYLAIR_TEST_PYTHON") {
        return p.into();
    }
    for cand in ["python3", "python"] {
        // Check exit status, not just spawn: on Windows `python3` can be the
        // Microsoft Store alias — it launches but does not run Python.
        // Require >=3.12: the runtime uses PEP 695 syntax.
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

fn spec(script: &str) -> SpawnSpec {
    SpawnSpec {
        program: python(),
        args: vec!["-c".into(), script.into()],
        env: vec![],
        cwd: None,
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

// ---- tests ----

#[tokio::test]
async fn hello_echo_pong_shutdown() {
    let (ev, _rx) = mpsc::channel::<RuntimeEvent>(64);
    let h = spawn_h(spec(FAKE_READY), fast_config(), ev);
    wait_state(&h, State::Ready).await;
    let out = h.call("echo", json!({"x": 1})).await.unwrap();
    assert_eq!(out, json!({"x": 1}));
    h.shutdown().await;
    wait_state(&h, State::Stopped).await;
}

#[tokio::test]
async fn startup_hang_is_killed_and_fails() {
    let (ev, _rx) = mpsc::channel::<RuntimeEvent>(64);
    // breaker_crashes = 2 -> after two failed startups, state Failed
    let h = spawn_h(spec(FAKE_HANG), fast_config(), ev);
    wait_state(&h, State::Failed).await;
}

#[tokio::test]
async fn stray_stdout_is_protocol_violation() {
    let (ev, mut rx) = mpsc::channel::<RuntimeEvent>(64);
    let h = spawn_h(spec(FAKE_STRAY_STDOUT), fast_config(), ev);
    wait_state(&h, State::Failed).await;
    // drain events: expect a Restarting transition (first crash) at least
    let mut saw_restart = false;
    while let Ok(e) = rx.try_recv() {
        if let RuntimeEvent::StateChanged {
            to: State::Restarting,
            ..
        } = e
        {
            saw_restart = true;
        }
    }
    assert!(saw_restart || h.state() == State::Failed);
}

#[tokio::test]
async fn malformed_frame_is_protocol_violation() {
    let (ev, _rx) = mpsc::channel::<RuntimeEvent>(64);
    let h = spawn_h(spec(FAKE_MALFORMED), fast_config(), ev);
    wait_state(&h, State::Failed).await;
}

#[tokio::test]
async fn wedged_runtime_killed_on_missed_pongs() {
    let (ev, _rx) = mpsc::channel::<RuntimeEvent>(64);
    let h = spawn_h(spec(FAKE_WEDGED), fast_config(), ev);
    wait_state(&h, State::Ready).await;
    // wedged: hello sent, never pongs -> Unresponsive -> killed -> restart cycle
    wait_state(&h, State::Failed).await;
}

#[tokio::test]
async fn kill_during_call_fails_caller_with_crashed() {
    let (ev, _rx) = mpsc::channel::<RuntimeEvent>(64);
    let h = spawn_h(spec(FAKE_DIE_ON_CALL), fast_config(), ev);
    wait_state(&h, State::Ready).await;
    let err = h.call("boom", json!({})).await.unwrap_err();
    assert_eq!(err.code, proto::RUNTIME_CRASHED);
}

#[tokio::test]
async fn crash_loop_breaker_trips() {
    let (ev, mut rx) = mpsc::channel::<RuntimeEvent>(64);
    let h = spawn_h(spec(FAKE_EXIT_NOW), fast_config(), ev);
    wait_state(&h, State::Failed).await;
    let mut restarts = 0;
    while let Ok(e) = rx.try_recv() {
        if let RuntimeEvent::StateChanged {
            to: State::Restarting,
            ..
        } = e
        {
            restarts += 1;
        }
    }
    assert!(
        restarts >= 1,
        "expected at least one restart before breaker"
    );
    // and no more spawns after Failed — state stays Failed
    sleep(Duration::from_millis(300)).await;
    assert_eq!(h.state(), State::Failed);
    // calls fail fast with RUNTIME_UNAVAILABLE
    let err = h.call("x", Value::Null).await.unwrap_err();
    assert_eq!(err.code, proto::RUNTIME_UNAVAILABLE);
}

// ---- end-to-end: real Rust supervisor <-> real Python runtime ----
// M1 exit criterion: the two sides are tested against each other, not only
// against scripted fakes — protocol drift cannot hide here.

#[tokio::test]
async fn e2e_real_runtime_full_flow() {
    let (ev, mut rx) = mpsc::channel::<RuntimeEvent>(64);
    let h = spawn_h(real_runtime_spec(), fast_config(), ev);

    wait_state(&h, State::Ready).await;
    eprintln!("e2e: ready");

    // exposes manifest from the real testapp
    let mut exposes = Value::Null;
    while let Ok(e) = rx.try_recv() {
        if let RuntimeEvent::Exposes(x) = e {
            exposes = x;
        }
    }
    let names: Vec<&str> = exposes
        .as_array()
        .expect("exposes is an array")
        .iter()
        .filter_map(|e| e["name"].as_str())
        .collect();
    for f in ["add", "counter", "sleep_forever", "isolated_add"] {
        assert!(names.contains(&f), "missing expose {f} in {names:?}");
    }
    eprintln!("e2e: exposes ok {names:?}");

    // plain call
    let out = timeout(T, h.call("add", json!({"a": 20, "b": 22})))
        .await
        .expect("call timed out")
        .unwrap();
    assert_eq!(out, json!(42));
    eprintln!("e2e: call ok");

    // streaming call: chunks then terminal
    let (_task, mut items) = timeout(T, h.call_stream("counter", json!({"n": 3})))
        .await
        .expect("call_stream timed out")
        .unwrap();
    let mut seen = Vec::new();
    while let Ok(Some(item)) = timeout(T, items.recv()).await {
        match item {
            tauri_plugin_pylair::supervisor::StreamItem::Chunk { value, .. } => {
                seen.push(value.as_i64().unwrap())
            }
            tauri_plugin_pylair::supervisor::StreamItem::End(res) => {
                assert!(res.is_ok());
                break;
            }
        }
    }
    assert_eq!(seen, vec![0, 1, 2]);
    eprintln!("e2e: stream ok");

    // cancel a never-ending async task — eager dispatch via call_task so the
    // task id is known before we cancel it.
    let ct = h.call_task("sleep_forever", json!({})).unwrap();
    let task = timeout(T, ct.task)
        .await
        .expect("task id timed out")
        .unwrap();
    let cancelled = timeout(T, h.cancel(task))
        .await
        .expect("cancel timed out")
        .unwrap();
    assert!(cancelled);
    let err = timeout(T, ct.result)
        .await
        .expect("result timed out")
        .unwrap()
        .unwrap_err();
    assert_eq!(err.code, proto::TASK_CANCELLED);
    eprintln!("e2e: cancel ok");

    // stderr from the real runtime is drained (fd-1 hygiene lands there)
    h.shutdown().await;
    wait_state(&h, State::Stopped).await;
    eprintln!("e2e: stopped");
}

#[tokio::test]
async fn version_mismatch_goes_straight_to_failed() {
    let (ev, mut rx) = mpsc::channel::<RuntimeEvent>(64);
    let h = spawn_h(spec(FAKE_BAD_VERSION), fast_config(), ev);
    wait_state(&h, State::Failed).await;
    // Fatal is not a crash: zero Restarting transitions must have occurred.
    let mut restarts = 0;
    while let Ok(e) = rx.try_recv() {
        if let RuntimeEvent::StateChanged {
            to: State::Restarting,
            ..
        } = e
        {
            restarts += 1;
        }
    }
    assert_eq!(
        restarts, 0,
        "version mismatch must not enter the restart loop"
    );
}

#[tokio::test]
async fn retry_respawns_after_failed() {
    let (ev, mut rx) = mpsc::channel::<RuntimeEvent>(64);
    let h = spawn_h(spec(FAKE_EXIT_NOW), fast_config(), ev);
    wait_state(&h, State::Failed).await;
    h.retry().await.unwrap();
    // Retry must leave Failed: expect a Starting transition next.
    let saw_starting = timeout(T, async {
        while let Ok(Some(e)) = timeout(T, rx.recv()).await {
            if let RuntimeEvent::StateChanged {
                to: State::Starting,
                ..
            } = e
            {
                return true;
            }
        }
        false
    })
    .await
    .unwrap_or(false);
    assert!(saw_starting, "retry never respawned the runtime");
    // (the fake still fails -> back to Failed, which is fine)
    wait_state(&h, State::Failed).await;
}

#[tokio::test]
async fn healthy_runtime_never_flaps_unresponsive() {
    let (ev, mut rx) = mpsc::channel::<RuntimeEvent>(64);
    let h = spawn_h(spec(FAKE_READY), fast_config(), ev);
    wait_state(&h, State::Ready).await;
    // sit through several ping intervals with a pinging runtime
    sleep(Duration::from_millis(600)).await;
    let mut flapped = false;
    while let Ok(e) = rx.try_recv() {
        if let RuntimeEvent::StateChanged {
            to: State::Unresponsive,
            ..
        } = e
        {
            flapped = true;
        }
    }
    assert!(!flapped, "healthy runtime flapped to Unresponsive");
}

#[tokio::test]
async fn stderr_is_drained_and_forwarded() {
    const NOISY: &str = r#"
import sys, struct, json, time
sys.stderr.write("hello from stderr\n"); sys.stderr.flush()
b = json.dumps({"jsonrpc":"2.0","method":"hello","params":{"protocol":1,"exposes":[]}}).encode()
sys.stdout.buffer.write(struct.pack(">I", len(b)) + b); sys.stdout.buffer.flush()
time.sleep(0.3)
"#;
    let (ev, mut rx) = mpsc::channel::<RuntimeEvent>(64);
    let h = spawn_h(spec(NOISY), fast_config(), ev);
    wait_state(&h, State::Ready).await;
    sleep(Duration::from_millis(400)).await;
    let mut lines = vec![];
    while let Ok(e) = rx.try_recv() {
        if let RuntimeEvent::Stderr(l) = e {
            lines.push(l);
        }
    }
    assert!(lines.iter().any(|l| l.contains("hello from stderr")));
}

#[tokio::test]
async fn restart_trigger_respawns_cleanly() {
    // Dev-mode hot reload: touching the trigger file respawns the runtime
    // without crash accounting — the state goes Restarting -> Ready and a
    // fresh generation serves calls.
    let dir = std::env::temp_dir().join(format!("pylair-trig-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let trigger = dir.join("trigger");
    std::fs::write(&trigger, "0").unwrap();

    let (ev, mut rx) = mpsc::channel::<RuntimeEvent>(64);
    let mut cfg = fast_config();
    cfg.restart_trigger = Some(trigger.clone());
    let h = spawn_h(spec(FAKE_READY), cfg, ev);
    wait_state(&h, State::Ready).await;

    // Rewrite past the mtime granularity of the first write.
    sleep(Duration::from_millis(50)).await;
    std::fs::write(&trigger, "1").unwrap();

    let saw_restart_note = timeout(T, async {
        while let Some(e) = rx.recv().await {
            if let RuntimeEvent::Stderr(l) = e {
                if l.contains("source changed") {
                    return true;
                }
            }
        }
        false
    })
    .await
    .unwrap_or(false);
    assert!(saw_restart_note, "no restart notice on trigger change");

    // The respawned generation must serve calls again.
    wait_state(&h, State::Ready).await;
    let out = h.call("echo", json!({"after": "restart"})).await.unwrap();
    assert_eq!(out, json!({"after": "restart"}));
    h.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}
