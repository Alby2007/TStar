//! M0 packaging spike: spawn the bundled Python, send one length-prefixed
//! JSON frame, return the reply. Deliberately spawn-per-call — the
//! supervisor (persistent process, restart policy) is M1's job.
//!
//! Frame format: 4-byte big-endian length + JSON body (see backend/app.py).

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use tauri::Manager;

struct BackendPaths {
    python: PathBuf,
    site_packages: PathBuf,
    script: PathBuf,
}

fn backend_paths(app: &tauri::AppHandle) -> Result<BackendPaths, String> {
    let python_rel: [&str; 3] = ["python", "bin", "python3"];
    let python_win: [&str; 2] = ["python", "python.exe"];

    let from = |base: PathBuf| -> BackendPaths {
        let python = if cfg!(windows) {
            python_win.iter().fold(base.clone(), |p, s| p.join(s))
        } else {
            python_rel.iter().fold(base.clone(), |p, s| p.join(s))
        };
        BackendPaths {
            python,
            site_packages: base.join("site-packages"),
            script: base.join("backend").join("app.py"),
        }
    };

    // Packaged: everything lands under the OS resource dir.
    let res = app.path().resource_dir().map_err(|e| e.to_string())?;
    let bundled = from(res);
    if bundled.python.exists() {
        return Ok(bundled);
    }

    // Dev: resources aren't assembled into a bundle yet; they're staged in
    // src-tauri/ by the fetch/prepare scripts. Same layout either way.
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    Ok(from(manifest))
}

#[tauri::command]
fn ping_python(app: tauri::AppHandle, message: String) -> Result<serde_json::Value, String> {
    let paths = backend_paths(&app)?;

    let mut child = Command::new(&paths.python)
        .arg(&paths.script)
        .env("PYTHONPATH", &paths.site_packages)
        .env("PYTHONNOUSERSITE", "1")
        .env("PYTHONUTF8", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to spawn {}: {e}", paths.python.display()))?;

    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "ping",
        "params": { "message": message },
    });
    let body = serde_json::to_vec(&request).map_err(|e| e.to_string())?;
    {
        let stdin = child.stdin.as_mut().ok_or("python stdin unavailable")?;
        stdin
            .write_all(&(body.len() as u32).to_be_bytes())
            .and_then(|()| stdin.write_all(&body))
            .map_err(|e| format!("failed to write request frame: {e}"))?;
    }

    let mut response = Vec::new();
    let mut stderr = String::new();
    let read_result = (|| -> Result<(), String> {
        let stdout = child.stdout.as_mut().ok_or("python stdout unavailable")?;
        let mut len_buf = [0u8; 4];
        stdout
            .read_exact(&mut len_buf)
            .map_err(|e| format!("reading frame length: {e}"))?;
        let n = u32::from_be_bytes(len_buf) as usize;
        response = vec![0u8; n];
        stdout
            .read_exact(&mut response)
            .map_err(|e| format!("reading frame body ({n} bytes): {e}"))
    })();

    if let Some(mut err) = child.stderr.take() {
        let _ = err.read_to_string(&mut stderr);
    }
    let _ = child.kill(); // spawn-per-call; nothing to supervise yet
    let _ = child.wait();

    read_result.map_err(|e| format!("{e}\npython stderr: {}", stderr.trim()))?;
    let value: serde_json::Value = serde_json::from_slice(&response)
        .map_err(|e| format!("invalid response frame: {e}\npython stderr: {}", stderr.trim()))?;
    Ok(value)
}

fn main() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![ping_python])
        .run(tauri::generate_context!())
        .expect("error while running m0-packaging-spike");
}
