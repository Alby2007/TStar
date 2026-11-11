//! Regression: plugin `setup` runs on the main thread with NO Tokio reactor
//! entered. A bare `tokio::spawn` there panicked every real app at launch
//! ("there is no reactor running"). This drives setup through Tauri's mock
//! runtime from a plain `#[test]` (deliberately not `#[tokio::test]`), so a
//! setup-context bug fails without needing a display.
//!
//! Not built on Windows: a test exe linking tauri's webview stack aborts at
//! load with STATUS_ENTRYPOINT_NOT_FOUND (0xc0000139, missing common-controls
//! manifest that a real app embeds via tauri-build). The bug itself is
//! OS-independent (tokio reactor context), so macOS + Linux cover it, and
//! Windows is covered by the acceptance job's real GUI self-test.
#![cfg(not(windows))]

use tauri::test::{mock_context, noop_assets, MockRuntime};
use tauri::Manager;
use tauri_plugin_pylair::{Builder, PyRuntime, SpawnSpec};

fn sleeper() -> SpawnSpec {
    let py = if cfg!(windows) { "python" } else { "python3" };
    SpawnSpec {
        program: py.into(),
        args: vec!["-c".into(), "import time; time.sleep(30)".into()],
        ..Default::default()
    }
}

#[test]
fn eager_spec_setup_does_not_panic_and_manages_runtime() {
    let app = tauri::test::mock_builder()
        .plugin(Builder::<MockRuntime>::new(sleeper()).build())
        .build(mock_context(noop_assets()))
        .expect("app builds with the plugin");
    assert!(app.try_state::<PyRuntime>().is_some());
}

#[test]
fn lazy_spec_setup_does_not_panic() {
    let app = tauri::test::mock_builder()
        .plugin(Builder::<MockRuntime>::new_with(|_app| sleeper()).build())
        .build(mock_context(noop_assets()))
        .expect("app builds with the lazy plugin");
    assert!(app.try_state::<PyRuntime>().is_some());
}
