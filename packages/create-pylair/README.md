# create-pylair

Scaffold a new Pylair desktop app — a Tauri shell, a managed Python
runtime, and a modern web frontend, wired together and ready to run.

```sh
npm create pylair my-app
```

**Release candidate:** while there's no stable release, `latest` tracks
the newest RC — the command above gives you the newest RC. Pin an exact
version for reproducibility: `npm create pylair@<version> my-app`.

## Prerequisites

- **Node** 22+
- **Rust** toolchain — on Windows, install the *Visual Studio Build
  Tools* "Desktop development with C++" workload first
- **uv** — for the Python environment
  (`pip install uv`, or see https://docs.astral.sh/uv/)

A first `pylair build` also needs GitHub access once: it downloads the
bundled Python runtime and Tauri's installer toolchain (WiX/NSIS on
Windows). Later builds reuse the caches.

## Capabilities

The scaffolded `src-tauri/capabilities/default.json` grants
`core:default` plus `pylair:default`, and additionally **denies**
`core:event:deny-emit` / `core:event:deny-emit-to`: a webview that
could emit could forge `pylair://stream` events into another window.
`@pylair/client` only listens, so a stock app loses nothing — but if
your app needs JS-side `emit`, add `"core:event:allow-emit"` back in
your own capability block (deny rules win, so remove the deny entries
too). See `docs/security.md` for the full event-scoping model.

Pylair builds on [Tauri](https://tauri.app) but is not affiliated with
or endorsed by the Tauri project.

Licensed under MIT OR Apache-2.0 — see LICENSE-MIT and LICENSE-APACHE.
