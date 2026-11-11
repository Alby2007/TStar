//! App wiring: picks the Python interpreter for dev vs bundled release.
//! Dev (`pylair dev`) exports PYLAIR_* env vars pointing at the project
//! venv; a packaged build resolves the interpreter under resource_dir.

use tauri::Manager;
use tauri_plugin_pylair::{Builder as Pylair, SpawnSpec};

const APP_MODULE: &str = "backend.app";

fn python_spec(app: &tauri::AppHandle) -> SpawnSpec {
    // ── dev: `pylair dev` supplies interpreter, module, source path ──
    if let Ok(module) = std::env::var("PYLAIR_APP_MODULE") {
        let program = std::env::var("PYLAIR_PYTHON").unwrap_or_else(|_| "python3".into());
        let mut spec = SpawnSpec::python(program.into(), &module);
        if let Ok(pp) = std::env::var("PYLAIR_PYTHONPATH") {
            spec.env.push(("PYTHONPATH".into(), pp));
        }
        if let Ok(trig) = std::env::var("PYLAIR_RESTART_TRIGGER") {
            spec.env.push(("PYLAIR_RESTART_TRIGGER".into(), trig));
        }
        return spec;
    }

    // ── release: everything bundled under the OS resource dir ──
    let res = app.path().resource_dir().expect("resource_dir");
    let python = res
        .join("python")
        .join(if cfg!(windows) { "python.exe" } else { "bin/python3" });
    let sep = if cfg!(windows) { ";" } else { ":" };
    let pythonpath = format!(
        "{}{}{}",
        res.join("backend").display(),
        sep,
        res.join("site-packages").display()
    );
    let mut spec = SpawnSpec::python(python, APP_MODULE);
    spec.env.push(("PYTHONPATH".into(), pythonpath));
    spec.env.push(("PYTHONUTF8".into(), "1".into()));
    spec.env.push(("PYTHONNOUSERSITE".into(), "1".into()));
    spec
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(Pylair::new_with(python_spec).build())
        .run(tauri::generate_context!())
        .expect("error while running application");
}
