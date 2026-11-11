//! Regression: plugin `setup` runs on the main thread with NO Tokio reactor
//! entered. A bare `tokio::spawn` there panicked every real app at launch
//! ("there is no reactor running"). This drives setup through Tauri's mock
//! runtime from a plain `#[test]` (deliberately not `#[tokio::test]`), so a
//! setup-context bug fails on every OS without needing a display.

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
