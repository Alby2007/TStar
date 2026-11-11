//! Tauri plugin: supervisor, IPC bridge and permission enforcement for the
//! managed Python runtime.
//!
//! `protocol` / `transport` / `supervisor` are Tauri-free and testable in
//! isolation. The plugin glue owns the supervisor `Handle`, forwards
//! lifecycle events to the frontend, and exposes the `python_*` commands.

pub mod protocol;
pub mod supervisor;
pub mod transport;

pub use protocol::ErrorObj;
pub use supervisor::{
    spawn as spawn_supervisor, Handle, RuntimeEvent, SpawnSpec, State, StreamItem, SupervisorConfig,
};

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use tauri::{Emitter, Manager, Runtime};

/// The interpreter spec for a packaged app, given the directory holding
/// `python/`, `site-packages/` and the app's source dir. Kept here (not in
/// app code) so path/env fixes reach every app via a plugin update.
pub fn release_spawn_spec(resources: &Path, app_module: &str, source_dir: &str) -> SpawnSpec {
    let python = resources.join("python").join(if cfg!(windows) {
        "python.exe"
    } else {
        "bin/python3"
    });
    let sep = if cfg!(windows) { ";" } else { ":" };
    // Import root = PARENT of the source dir: "backend.app" resolves from
    // resources/, not resources/backend/ (mirrors cli `importRoot`).
    let import_root = resources.join(Path::new(source_dir).parent().unwrap_or(Path::new("")));
    let pythonpath = format!(
        "{}{}{}",
        import_root.display(),
        sep,
        resources.join("site-packages").display()
    );
    let mut spec = SpawnSpec::pylair(python, app_module);
    spec.env.push(("PYTHONPATH".into(), pythonpath));
    spec.env.push(("PYTHONUTF8".into(), "1".into()));
    spec.env.push(("PYTHONNOUSERSITE".into(), "1".into()));
    spec
}

/// Dev-or-release spec: `pylair dev` exports PYLAIR_APP_MODULE / PYLAIR_PYTHON
/// / PYLAIR_PYTHONPATH / PYLAIR_RESTART_TRIGGER → the project venv; a packaged
/// build resolves `resource_dir`. Apps use this as the plugin's spec fn.
pub fn app_spawn_spec<R: Runtime>(
    app: &tauri::AppHandle<R>,
    app_module: &str,
    source_dir: &str,
) -> SpawnSpec {
    if let Ok(module) = std::env::var("PYLAIR_APP_MODULE") {
        let program = std::env::var("PYLAIR_PYTHON").unwrap_or_else(|_| "python3".into());
        let mut spec = SpawnSpec::pylair(PathBuf::from(program), &module);
        if let Ok(pp) = std::env::var("PYLAIR_PYTHONPATH") {
            spec.env.push(("PYTHONPATH".into(), pp));
        }
        // NB: PYLAIR_RESTART_TRIGGER is read by the supervisor from its own
        // env (inherited from `pylair dev`) — not forwarded to the child.
        return spec;
    }
    let res = app.path().resource_dir().expect("resource_dir");
    release_spawn_spec(&res, app_module, source_dir)
}

// ── bundled self-test ───────────────────────────────────────────────
// Release-path verification apps can ship: `pylair-self-test` resolves the
// bundled resources exactly as the packaged app sees them, spawns the real
// supervisor, makes one real call, prints, exits. Two entry points:
//   headless — before any AppHandle exists (no window, no Tauri loop);
//   gui      — inside setup(), exercising app.path().resource_dir() itself.

fn resources_from_exe() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    let stem = exe.file_stem()?.to_string_lossy();
    [
        dir.join("..").join("lib").join(stem.as_ref()), // deb/AppImage usr/lib
        dir.join("..").join("Resources"),               // macOS .app
        dir.to_path_buf(),                              // NSIS install root
    ]
    .into_iter()
    .find(|p| p.join("python").exists())
}

async fn self_test_call(spec: SpawnSpec, origin: &str) -> i32 {
    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    let (h, actor) = spawn_supervisor(spec, SupervisorConfig::default(), tx);
    tokio::spawn(actor);
    // Surface the runtime's stderr and lifecycle: a self-test that only says
    // "never reached Ready" is undiagnosable in CI.
    tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            match ev {
                RuntimeEvent::Stderr(line) => eprintln!("[runtime stderr] {line}"),
                RuntimeEvent::StateChanged { from, to, detail } => {
                    eprintln!("[runtime state] {from:?} -> {to:?}: {detail}")
                }
                RuntimeEvent::Exposes(_) => {}
            }
        }
    });
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
        Ok(true) => match h.call("hello", json!({"name": "self-test"})).await {
            Ok(v) => {
                println!("self-test({origin}) ok: {v}");
                h.shutdown().await;
                0
            }
            Err(e) => {
                eprintln!("self-test({origin}): call failed: {e:?}");
                3
            }
        },
        _ => {
            eprintln!(
                "self-test({origin}): never reached Ready (state: {:?})",
                h.state()
            );
            4
        }
    }
}

/// Headless: resources located relative to the executable — runs on a bare
/// machine with no display and no system Python. Returns an exit code.
pub fn self_test_headless(app_module: &str, source_dir: &str) -> i32 {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("self-test: cannot build tokio runtime: {e}");
            return 2;
        }
    };
    rt.block_on(async {
        match resources_from_exe() {
            Some(res) => {
                self_test_call(release_spawn_spec(&res, app_module, source_dir), "headless").await
            }
            None => {
                eprintln!("self-test: no bundled resources near the executable");
                2
            }
        }
    })
}

/// GUI: runs inside the app's setup() — exercises resource_dir() itself.
/// Spawn on the Tauri runtime; exit the app with the returned code.
pub async fn self_test_gui<R: Runtime>(
    app: &tauri::AppHandle<R>,
    app_module: &str,
    source_dir: &str,
) -> i32 {
    let spec = app_spawn_spec(app, app_module, source_dir);
    self_test_call(spec, "gui").await
}

/// True when the process should run the headless self-test instead of the app.
pub fn self_test_requested() -> bool {
    std::env::args().any(|a| a == "--self-test") || std::env::var("PYLAIR_SELF_TEST").is_ok()
}

/// Frontend event names (kept in one place — the client SDK mirrors them).
pub mod events {
    /// `{from, to, detail}` lifecycle transitions.
    pub const STATE: &str = "pylair://state";
    /// One line of runtime stderr.
    pub const STDERR: &str = "pylair://stderr";
    /// The exposes manifest after handshake.
    pub const EXPOSES: &str = "pylair://exposes";
    /// Stream items: `{task, seq?, value?, done, result?|error?}`.
    pub const STREAM: &str = "pylair://stream";
}

/// Managed state holding the supervisor handle.
pub struct PyRuntime(pub Handle);

/// Plugin builder — the app supplies where Python lives, eagerly or lazily.
/// `new` takes a fixed spec; `new_with` defers it to setup time, which is how
/// apps resolve the bundled interpreter under `resource_dir` at release while
/// using a dev venv when `PYLAIR_APP_MODULE` is set.
type SpecFn<R> = Box<dyn Fn(&tauri::AppHandle<R>) -> SpawnSpec + Send + Sync>;

pub struct Builder<R: Runtime = tauri::Wry> {
    spec: SpawnSpec,
    lazy: Option<SpecFn<R>>,
    config: SupervisorConfig,
}

impl<R: Runtime> Builder<R> {
    pub fn new(spec: SpawnSpec) -> Self {
        Self {
            spec,
            lazy: None,
            config: SupervisorConfig::default(),
        }
    }

    /// Spec computed at plugin setup — receives the AppHandle so it can
    /// resolve `path().resource_dir()`.
    pub fn new_with(f: impl Fn(&tauri::AppHandle<R>) -> SpawnSpec + Send + Sync + 'static) -> Self {
        Self {
            spec: SpawnSpec::default(),
            lazy: Some(Box::new(f)),
            config: SupervisorConfig::default(),
        }
    }

    pub fn config(mut self, config: SupervisorConfig) -> Self {
        self.config = config;
        self
    }

    pub fn build(self) -> tauri::plugin::TauriPlugin<R> {
        let mut plugin = tauri::plugin::Builder::<R>::new("pylair");
        // Installed-webview self-test: the client sees this flag and runs its
        // checks through the real bridge, then calls python_selftest_report.
        if std::env::var_os("PYLAIR_SELF_TEST_WEBVIEW").is_some() {
            plugin = plugin.js_init_script("window.__PYLAIR_SELF_TEST__ = true;".to_string());
        }
        plugin
            .setup(move |app, _api| {
                let spec = self.lazy.as_ref().map(|f| f(app)).unwrap_or_else(|| self.spec.clone());
                let (ev_tx, mut ev_rx) = tokio::sync::mpsc::channel::<RuntimeEvent>(256);
                // setup runs on the main thread with no reactor entered —
                // spawn the actor via tauri's runtime, not bare tokio::spawn.
                let (handle, actor) = spawn_supervisor(spec, self.config.clone(), ev_tx);
                tauri::async_runtime::spawn(actor);
                app.manage(PyRuntime(handle));

                let app_handle = app.clone();
                tauri::async_runtime::spawn(async move {
                    while let Some(ev) = ev_rx.recv().await {
                        let _ = match ev {
                            RuntimeEvent::StateChanged { from, to, detail } => app_handle.emit(
                                events::STATE,
                                json!({"from": fmt_state(from), "to": fmt_state(to), "detail": detail}),
                            ),
                            RuntimeEvent::Stderr(line) => app_handle.emit(events::STDERR, line),
                            RuntimeEvent::Exposes(x) => app_handle.emit(events::EXPOSES, x),
                        };
                    }
                });
                Ok(())
            })
            .on_drop(|app| {
                if let Some(rt) = app.try_state::<PyRuntime>() {
                    let handle = rt.0.clone();
                    tauri::async_runtime::spawn(async move { handle.shutdown().await });
                }
            })
            .invoke_handler(tauri::generate_handler![
                python_call,
                python_call_stream,
                python_grant_credit,
                python_cancel,
                python_retry,
                python_state,
                python_shutdown,
                python_selftest_report,
            ])
            .build()
    }
}

fn fmt_state(s: State) -> &'static str {
    match s {
        State::Starting => "starting",
        State::Ready => "ready",
        State::Unresponsive => "unresponsive",
        State::Restarting => "restarting",
        State::Failed => "failed",
        State::Stopped => "stopped",
    }
}

#[tauri::command]
async fn python_call(
    rt: tauri::State<'_, PyRuntime>,
    fn_name: String,
    input: Value,
) -> Result<Value, ErrorObj> {
    rt.0.call(&fn_name, input).await
}

/// Start a streaming call. Returns the task id; chunks and the terminal
/// arrive on the `pylair://stream` event.
#[tauri::command]
async fn python_call_stream<R: Runtime>(
    app: tauri::AppHandle<R>,
    rt: tauri::State<'_, PyRuntime>,
    fn_name: String,
    input: Value,
) -> Result<u64, ErrorObj> {
    let (task, mut items) = rt.0.call_stream(&fn_name, input).await?;
    tauri::async_runtime::spawn(async move {
        while let Some(item) = items.recv().await {
            let payload = match item {
                StreamItem::Chunk { seq, value } => {
                    json!({"task": task, "seq": seq, "value": value, "done": false})
                }
                StreamItem::End(Ok(result)) => {
                    json!({"task": task, "done": true, "result": result})
                }
                StreamItem::End(Err(e)) => {
                    json!({"task": task, "done": true, "error": e})
                }
            };
            let _ = app.emit(events::STREAM, payload);
        }
    });
    Ok(task)
}

#[tauri::command]
async fn python_grant_credit(
    rt: tauri::State<'_, PyRuntime>,
    task: u64,
    n: u64,
) -> Result<(), ErrorObj> {
    rt.0.grant_credit(task, n).await
}

/// End of the installed-webview self-test: print the verdict and exit with
/// it. Inert unless PYLAIR_SELF_TEST_WEBVIEW is set, so shipping the
/// permission in a release app grants nothing.
#[tauri::command]
fn python_selftest_report<R: Runtime>(
    app: tauri::AppHandle<R>,
    result: Value,
) -> Result<(), ErrorObj> {
    if std::env::var_os("PYLAIR_SELF_TEST_WEBVIEW").is_none() {
        return Err(ErrorObj {
            code: protocol::INVALID_REQUEST,
            message: "webview self-test not enabled".into(),
            data: None,
        });
    }
    let ok = result.get("ok").and_then(Value::as_bool).unwrap_or(false);
    println!("webview-self-test: {result}");
    app.exit(if ok { 0 } else { 5 });
    Ok(())
}

#[tauri::command]
async fn python_cancel(rt: tauri::State<'_, PyRuntime>, task: u64) -> Result<bool, ErrorObj> {
    rt.0.cancel(task).await
}

/// Operator-initiated recovery: respawn the runtime out of the Failed state.
#[tauri::command]
async fn python_retry(rt: tauri::State<'_, PyRuntime>) -> Result<(), ErrorObj> {
    rt.0.retry().await
}

#[tauri::command]
fn python_state(rt: tauri::State<'_, PyRuntime>) -> &'static str {
    fmt_state(rt.0.state())
}

#[tauri::command]
async fn python_shutdown(rt: tauri::State<'_, PyRuntime>) -> Result<(), ErrorObj> {
    rt.0.shutdown().await;
    Ok(())
}
