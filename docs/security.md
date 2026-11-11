# Security model (stub)

To be written alongside M1 (supervisor + protocol). Required content:

- Threat model: the front end (webview) is the least-trusted layer.
- Only functions explicitly marked `@expose` in Python are callable; there
  is no arbitrary command execution path.
- How Tauri's capability/permission model is used to gate plugin commands.
- What the bridge does and does not allow the webview to reach (fs, env,
  network on the Python side).
- Handling of malformed frames from either direction.

See PROJECT_BRIEF.md section 4, "Security".
