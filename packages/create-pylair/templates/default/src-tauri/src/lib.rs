//! App wiring. The dev-vs-release interpreter spec lives in the plugin
//! (`app_spawn_spec`); `--self-test` exercises the RELEASE path headlessly:
//! resolve resources from the exe location, spawn the real supervisor,
//! make one call, print the result, exit — no window, no Tauri loop.

use std::path::PathBuf;
use tauri_plugin_pylair::{app_spawn_spec, release_spawn_spec};

const APP_MODULE: &str = "backend.app";
const SOURCE_DIR: &str = "backend";

/// Resources sit at a fixed offset from the executable per bundle layout:
/// deb/AppImage: usr/bin/app → usr/lib/app; macOS: Contents/MacOS →
/// Contents/Resources; Windows NSIS: install dir root.
fn resources_from_exe() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    let stem = exe.file_stem()?.to_string_lossy();
    [
        dir.join("..").join("lib").join(stem.as_ref()),
        dir.join("..").join("Resources"),
        dir.to_path_buf(),
    ]
    .into_iter()
    .find(|p| p.join("python").exists())
}

fn self_test() -> ! {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let code = rt.block_on(async {
        use tauri_plugin_pylair::{spawn_supervisor, State, SupervisorConfig};
        let res = match resources_from_exe() {
            Some(r) => r,
            None => {
                eprintln!("self-test: no bundled resources near the executable");
                return 2;
            }
        };
        let spec = release_spawn_spec(&res, APP_MODULE, SOURCE_DIR);
        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        let h = spawn_supervisor(spec, SupervisorConfig::default(), tx);
        let mut rx = h.subscribe_state();
        let ready = tokio::time::timeout(std::time::Duration::from_secs(60), async {
            while *rx.borrow() != State::Ready {
                if rx.changed().await.is_err() {
                    break;
                }
            }
            *rx.borrow() == State::Ready
        })
        .await;
        match ready {
            Ok(true) => match h.call("hello", serde_json::json!({"name": "self-test"})).await {
                Ok(v) => {
                    println!("self-test ok: {v}");
                    h.shutdown().await;
                    0
                }
                Err(e) => {
                    eprintln!("self-test: call failed: {e:?}");
                    3
                }
            },
            _ => {
                eprintln!("self-test: runtime never reached Ready (state: {:?})", h.state());
                4
            }
        }
    });
    std::process::exit(code)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    if std::env::args().any(|a| a == "--self-test") || std::env::var("PYLAIR_SELF_TEST").is_ok() {
        self_test();
    }
    tauri::Builder::default()
        .plugin(
            tauri_plugin_pylair::Builder::new_with(|app| {
                app_spawn_spec(app, APP_MODULE, SOURCE_DIR)
            })
            .build(),
        )
        .run(tauri::generate_context!())
        .expect("error while running application");
}
