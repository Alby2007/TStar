//! P1-8 event scoping: runtime events go to subscribed webviews only.
//! Two windows, one subscribed — the supervisor's Failed-state event must
//! land only on the subscribed window's listener.
//!
//! Not built on Windows (same webview-link constraint as plugin_setup).
#![cfg(not(windows))]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tauri::test::{mock_context, noop_assets, MockRuntime};
use tauri::{Listener, Manager, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_pylair::{events, Builder, Registry, SpawnSpec, SupervisorConfig};

fn sleeper() -> SpawnSpec {
    SpawnSpec {
        program: "python3".into(),
        args: vec!["-c".into(), "import time; time.sleep(30)".into()],
        ..Default::default()
    }
}

#[test]
fn runtime_events_reach_only_subscribed_windows() {
    let cfg = SupervisorConfig {
        hello_timeout: Duration::from_millis(300),
        ..Default::default()
    };
    let app = tauri::test::mock_builder()
        .plugin(Builder::<MockRuntime>::new(sleeper()).config(cfg).build())
        .build(mock_context(noop_assets()))
        .expect("app builds with the plugin");
    let w1 = WebviewWindowBuilder::new(&app, "w1", WebviewUrl::default())
        .build()
        .unwrap();
    let w2 = WebviewWindowBuilder::new(&app, "w2", WebviewUrl::default())
        .build()
        .unwrap();

    let heard1 = Arc::new(AtomicBool::new(false));
    let heard2 = Arc::new(AtomicBool::new(false));
    let h1 = heard1.clone();
    let h2 = heard2.clone();
    w1.listen(events::STATE, move |_| h1.store(true, Ordering::SeqCst));
    w2.listen(events::STATE, move |_| h2.store(true, Ordering::SeqCst));

    // Only w1 uses the bridge.
    app.state::<Registry>().lock().unwrap().subscribe("w1");

    // The sleeper never sends hello -> hello_timeout -> Failed state event.
    // Give the actor generous time; the deadline is 300ms.
    std::thread::sleep(Duration::from_secs(3));
    assert!(
        heard1.load(Ordering::SeqCst),
        "subscribed window must get state events"
    );
    assert!(
        !heard2.load(Ordering::SeqCst),
        "unsubscribed window must NOT get state events"
    );
}
