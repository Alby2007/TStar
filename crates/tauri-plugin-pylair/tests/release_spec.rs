//! The release spawn spec must make `<pkg>.<mod>` importable regardless of
//! the process cwd. Regression: PYTHONPATH pointed at resources/backend, so
//! `import backend.app` only worked when launched from the right directory.

use std::process::Command;
use tauri_plugin_pylair::release_spawn_spec;

#[test]
fn release_pythonpath_resolves_app_module_from_unrelated_cwd() {
    let res = std::env::temp_dir().join(format!("pylair-relspec-{}", std::process::id()));
    let pkg = res.join("backend");
    std::fs::create_dir_all(&pkg).unwrap();
    std::fs::write(pkg.join("__init__.py"), "").unwrap();
    std::fs::write(pkg.join("app.py"), "VALUE = 7\n").unwrap();
    std::fs::create_dir_all(res.join("site-packages")).unwrap();

    let spec = release_spawn_spec(&res, "backend.app", "backend");
    let pythonpath = spec
        .env
        .iter()
        .find(|(k, _)| k == "PYTHONPATH")
        .map(|(_, v)| v.clone())
        .expect("spec sets PYTHONPATH");

    let py = if cfg!(windows) { "python" } else { "python3" };
    let elsewhere = std::env::temp_dir().join("pylair-relspec-cwd");
    std::fs::create_dir_all(&elsewhere).unwrap();
    let out = Command::new(py)
        .current_dir(&elsewhere)
        .env("PYTHONPATH", &pythonpath)
        .env("PYTHONNOUSERSITE", "1")
        .args(["-c", "import backend.app; print(backend.app.VALUE)"])
        .output()
        .expect("python runs");
    let _ = std::fs::remove_dir_all(&res);
    assert!(
        out.status.success(),
        "import failed with PYTHONPATH={pythonpath}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "7");
}

#[test]
fn release_spec_strips_inherited_python_config() {
    // An AppImage's AppRun exports PYTHONHOME at the AppDir; inheriting it
    // broke the bundled interpreter's stdlib lookup (found by the Linux
    // acceptance run). The release spec must remove it from the child env.
    let spec = release_spawn_spec(std::path::Path::new("/r"), "backend.app", "backend");
    for k in ["PYTHONHOME", "PYTHONSTARTUP", "PYTHONEXECUTABLE"] {
        assert!(spec.env_remove.iter().any(|x| x == k), "{k} not stripped");
    }
}
