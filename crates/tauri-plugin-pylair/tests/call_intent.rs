//! rc.15 PR I — the host tells the runtime which client intent a call
//! carries. `Command::Call` (plain call) writes `"stream": false`;
//! `Command::CallStream` writes `"stream": true`. The runtime uses the
//! flag to reject call()/stream() mismatches with an actionable error;
//! without it a `call` request is ambiguous.

use std::path::PathBuf;
use std::time::Duration;
use tauri_plugin_pylair::supervisor::{
    spawn, Handle, RuntimeEvent, SpawnSpec, State, StreamItem, SupervisorConfig,
};
use tokio::sync::mpsc;
use tokio::time::timeout;

const T: Duration = Duration::from_secs(30);

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

fn spec(script: &str) -> SpawnSpec {
    SpawnSpec {
        program: python(),
        args: vec!["-c".into(), script.into()],
        env: vec![],
        cwd: None,
        ..Default::default()
    }
}

fn cfg() -> SupervisorConfig {
    let mut c = SupervisorConfig::default();
    c.hello_timeout = Duration::from_secs(10);
    c.ping_interval = Duration::from_secs(60);
    c.missed_pongs_to_kill = 1000;
    c.shutdown_grace = Duration::from_secs(2);
    c
}

async fn wait_state(h: &Handle, want: State) {
    let mut rx = h.subscribe_state();
    timeout(T, async {
        while *rx.borrow() != want {
            rx.changed().await.expect("state channel closed");
        }
    })
    .await
    .unwrap_or_else(|_| panic!("never reached {want:?}"));
}

/// Echo the `stream` field of every `call` request back as its result.
const ECHO_STREAM_FLAG: &str = r#"
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
    if m.get("method") == "ping":
        w({"jsonrpc":"2.0","method":"pong","params":{"seq": m["params"]["seq"]}})
    elif m.get("method") == "shutdown":
        w({"jsonrpc":"2.0","id": m["id"],"result": None}); break
    elif m.get("method") == "call":
        w({"jsonrpc":"2.0","id": m["id"],
            "result": {"stream": m["params"].get("stream")}})
"#;

#[tokio::test]
async fn plain_call_sends_stream_false_and_call_stream_sends_true() {
    let (ev, rx) = mpsc::channel(64);
    let h = spawn_h(spec(ECHO_STREAM_FLAG), cfg(), ev);
    wait_state(&h, State::Ready).await;

    let v = timeout(T, h.call("echo", serde_json::json!({}), None))
        .await
        .expect("call timed out")
        .expect("call failed");
    assert_eq!(
        v["stream"],
        serde_json::json!(false),
        "a plain call must declare stream:false"
    );

    let (_task, mut items) = timeout(T, h.call_stream("echo", serde_json::json!({}), None))
        .await
        .expect("call_stream timed out")
        .expect("call_stream failed");
    let mut seen = None;
    while let Ok(Some(item)) = timeout(T, items.recv()).await {
        if let StreamItem::End(r) = item {
            seen = Some(r.expect("stream terminal error"));
        }
    }
    assert_eq!(
        seen.expect("no terminal item")["stream"],
        serde_json::json!(true),
        "a call_stream must declare stream:true"
    );

    drop(rx);
    let _ = h.shutdown().await;
}
