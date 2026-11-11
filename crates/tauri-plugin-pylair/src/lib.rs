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
    let pythonpath = format!(
        "{}{}{}",
        resources.join(source_dir).display(),
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
        tauri::plugin::Builder::<R>::new("pylair")
            .setup(move |app, _api| {
                let spec = self.lazy.as_ref().map(|f| f(app)).unwrap_or_else(|| self.spec.clone());
                let (ev_tx, mut ev_rx) = tokio::sync::mpsc::channel::<RuntimeEvent>(256);
                let handle = spawn_supervisor(spec, self.config.clone(), ev_tx);
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
