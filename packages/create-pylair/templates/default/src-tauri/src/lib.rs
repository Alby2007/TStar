//! App wiring. The interpreter spec and the bundled self-test live in the
//! plugin — fixes there reach every app via a plugin update.
//!
//! `--self-test` / PYLAIR_SELF_TEST: headless release-path check (no window).
//! PYLAIR_SELF_TEST_GUI: same check inside setup(), through resource_dir().

use tauri_plugin_pylair::{app_spawn_spec, self_test_gui, self_test_headless, self_test_requested};

const APP_MODULE: &str = "backend.app";
const SOURCE_DIR: &str = "backend";

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    if self_test_requested() {
        std::process::exit(self_test_headless(APP_MODULE, SOURCE_DIR));
    }
    let gui_test = std::env::var("PYLAIR_SELF_TEST_GUI").is_ok();
    tauri::Builder::default()
        .plugin(
            tauri_plugin_pylair::Builder::new_with(|app| {
                app_spawn_spec(app, APP_MODULE, SOURCE_DIR)
            })
            .build(),
        )
        .setup(move |app| {
            if gui_test {
                let app = app.clone();
                tauri::async_runtime::spawn(async move {
                    let code = self_test_gui(&app, APP_MODULE, SOURCE_DIR).await;
                    app.exit(code);
                });
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running application");
}
