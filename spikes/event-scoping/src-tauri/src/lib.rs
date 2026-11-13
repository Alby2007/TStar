//! Two-window repro: the plugin emits `pylair://stream` via
//! `emit_to(owning label)`, the JS client listens with the default
//! `{kind:"Any"}` target. Every line a webview sees is mirrored back
//! through `repro-log` so it lands in out.log next to this crate —
//! the webview console is invisible when launched from a script.

use std::io::Write;
use std::sync::{Arc, Mutex};
use tauri::Listener;

const APP_MODULE: &str = "testapp";

pub fn run() {
    tauri::Builder::default()
        .plugin(
            tauri_plugin_pylair::Builder::new_with(|app| {
                tauri_plugin_pylair::app_spawn_spec(app, APP_MODULE, "backend")
            })
            .build(),
        )
        .setup(|app| {
            let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../out.log");
            let out = Arc::new(Mutex::new(
                std::fs::File::create(&path).expect("open out.log"),
            ));
            app.listen("repro-log", move |ev: tauri::Event| {
                let mut f = out.lock().unwrap();
                let _ = writeln!(f, "{}", ev.payload());
                let _ = f.flush();
            });
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running repro");
}
