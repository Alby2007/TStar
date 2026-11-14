//! F5 (rc.15): configurable hello_timeout and the "still starting" signal.
//!
//!   * A runtime that takes longer than half the hello timeout produces
//!     exactly one repeated `starting → starting` state event carrying a
//!     human-readable detail, then still reaches Ready — the signal is a
//!     hint for UIs, not a state change.
//!   * A runtime that never hellos fails with a message that names both
//!     escape hatches (env knob and Builder method).
//!   * A fast hello emits no same-state event at all — normal boots are
//!     silent.

use std::path::PathBuf;
use std::time::Duration;
use tauri_plugin_pylair::supervisor::{
    spawn, Handle, RuntimeEvent, SpawnSpec, State, SupervisorConfig,
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

fn cfg(hello_timeout: Duration) -> SupervisorConfig {
    let mut c = SupervisorConfig::default();
    c.hello_timeout = hello_timeout;
    c.ping_interval = Duration::from_secs(60); // no liveness interference
    c.missed_pongs_to_kill = 1000;
    c.backoff_base = Duration::from_millis(20);
    c.backoff_cap = Duration::from_millis(80);
    c.breaker_crashes = 50;
    c.shutdown_grace = Duration::from_secs(2);
    c.loop_stall_kill = None;
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
    .unwrap_or_else(|_| panic!("never reached {want:?} (state {:?})", *rx.borrow()));
}

/// No hello at all: the timeout message must name both escape hatches.
#[tokio::test]
async fn hello_timeout_message_names_env_and_builder_knobs() {
    const NEVER_HELLO: &str = "import time; time.sleep(999)";
    let (ev, mut rx) = mpsc::channel::<RuntimeEvent>(256);
    let h = spawn_h(spec(NEVER_HELLO), cfg(Duration::from_secs(2)), ev);
    let found = timeout(Duration::from_secs(15), async {
        loop {
            match rx.recv().await {
                Some(RuntimeEvent::StateChanged { detail, .. })
                    if detail.contains("PYLAIR_HELLO_TIMEOUT_MS")
                        && detail.contains("Builder::hello_timeout") =>
                {
                    break detail;
                }
                Some(_) => continue,
                None => panic!("events channel closed before timeout fired"),
            }
        }
    })
    .await
    .expect("no event named the hello-timeout knobs within 15s");
    assert!(
        found.contains("no hello"),
        "timeout detail should still explain what happened: {found}"
    );
    h.shutdown().await;
}
