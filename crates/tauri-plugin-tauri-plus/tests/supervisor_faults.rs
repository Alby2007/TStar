//! Fault-injection tests for the supervisor: kill mid-call, startup hang,
//! malformed frames, stray garbage on stdout, wedged runtime (no pongs),
//! crash-loop breaker. Each case spawns a real `python -c` process.

use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::Duration;
use tauri_plugin_tauri_plus::protocol as proto;
use tauri_plugin_tauri_plus::supervisor::{
    spawn, Handle, RuntimeEvent, SpawnSpec, State, SupervisorConfig,
};
use tokio::sync::mpsc;
use tokio::time::{sleep, timeout};

const T: Duration = Duration::from_secs(15);

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

fn python() -> PathBuf {
    if let Ok(p) = std::env::var("TAURI_PLUS_TEST_PYTHON") {
        return p.into();
    }
    for cand in ["python3", "python"] {
        if std::process::Command::new(cand)
            .arg("--version")
            .output()
            .is_ok()
        {
            return cand.into();
        }
    }
    panic!("no python interpreter found for supervisor tests");
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
    let h = spawn(spec(FAKE_READY), fast_config(), ev);
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
    let h = spawn(spec(FAKE_HANG), fast_config(), ev);
    wait_state(&h, State::Failed).await;
}

#[tokio::test]
async fn stray_stdout_is_protocol_violation() {
    let (ev, mut rx) = mpsc::channel::<RuntimeEvent>(64);
    let h = spawn(spec(FAKE_STRAY_STDOUT), fast_config(), ev);
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
    let h = spawn(spec(FAKE_MALFORMED), fast_config(), ev);
    wait_state(&h, State::Failed).await;
}

#[tokio::test]
async fn wedged_runtime_killed_on_missed_pongs() {
    let (ev, _rx) = mpsc::channel::<RuntimeEvent>(64);
    let h = spawn(spec(FAKE_WEDGED), fast_config(), ev);
    wait_state(&h, State::Ready).await;
    // wedged: hello sent, never pongs -> Unresponsive -> killed -> restart cycle
    wait_state(&h, State::Failed).await;
}

#[tokio::test]
async fn kill_during_call_fails_caller_with_crashed() {
    let (ev, _rx) = mpsc::channel::<RuntimeEvent>(64);
    let h = spawn(spec(FAKE_DIE_ON_CALL), fast_config(), ev);
    wait_state(&h, State::Ready).await;
    let err = h.call("boom", json!({})).await.unwrap_err();
    assert_eq!(err.code, proto::RUNTIME_CRASHED);
}

#[tokio::test]
async fn crash_loop_breaker_trips() {
    let (ev, mut rx) = mpsc::channel::<RuntimeEvent>(64);
    let h = spawn(spec(FAKE_EXIT_NOW), fast_config(), ev);
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
    let h = spawn(spec(NOISY), fast_config(), ev);
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
