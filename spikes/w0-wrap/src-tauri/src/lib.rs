//! W0 wrap spike: spawn the launcher under the real supervisor, navigate
//! the window to the app URL when `READY <url>` appears on stderr.
//! Throwaway — no plugin changes (that's what W0 is testing).

use tauri::{Emitter, Listener, Manager};
use tauri_plugin_pylair::{events, SpawnSpec};

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let app_py = std::env::var("W0_APP").unwrap_or_else(|_| "fastapi".into());
    let cmd: Vec<String> = match app_py.as_str() {
        "gradio" => vec!["python".into(), "apps/gradio_app.py".into()],
        "streamlit" => vec![
            "python".into(),
            "-m".into(),
            "streamlit".into(),
            "run".into(),
            "apps/streamlit_app.py".into(),
            "--server.address".into(),
            "127.0.0.1".into(),
            "--server.port".into(),
            "{port}".into(),
            "--server.headless".into(),
            "true".into(),
            "--server.fileWatcherType".into(),
            "none".into(),
        ],
        _ => vec![
            "python".into(),
            "apps/fastapi_app.py".into(),
        ],
    };

    // args = [launcher.py, --, cmd...]
    let mut spec = SpawnSpec {
        program: "python".into(),
        args: std::iter::once("launcher.py".into())
            .chain(std::iter::once("--".into()))
            .chain(cmd.into_iter())
            .collect(),
        ..Default::default()
    };
    spec.env = vec![
        ("PYTHONPATH".into(), ".".into()),
        ("PYTHONUTF8".into(), "1".into()),
    ];
    spec.cwd = Some(std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".."));

    tauri::Builder::default()
        .plugin(tauri_plugin_pylair::Builder::new(spec).build())
        .setup(|app| {
            let win = app.get_webview_window("main").expect("main window");
            let app_handle = app.handle().clone();
            app.listen(events::STDERR, move |ev| {
                if let Some(url) = ev.payload().strip_prefix("READY ") {
                    let url = url.trim().to_string();
                    if let Ok(parsed) = url.parse::<url::Url>() {
                        let _ = win.navigate(parsed);
                    }
                    let _ = app_handle.emit("w0://ready", url);
                }
            });
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running application");
}
