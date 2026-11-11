# create-pylair

Scaffold a new Pylair desktop app — a Tauri shell, a managed Python
runtime, and a modern web frontend, wired together and ready to run.

```sh
npm create pylair my-app
```

**Release candidate:** npm sets `latest` to the first published version,
so the command above currently hands out the RC. Pin explicitly if you
want reproducibility — `npm create pylair@0.1.0-rc.2 my-app` — or use
the `next` tag, which tracks prereleases: `npm create pylair@next`.

## Prerequisites

- **Node** 22+
- **Rust** toolchain — on Windows, install the *Visual Studio Build
  Tools* "Desktop development with C++" workload first
- **uv** — for the Python environment
  (`pip install uv`, or see https://docs.astral.sh/uv/)

A first `pylair build` also needs GitHub access once: it downloads the
bundled Python runtime and Tauri's installer toolchain (WiX/NSIS on
Windows). Later builds reuse the caches.

Pylair builds on [Tauri](https://tauri.app) but is not affiliated with
or endorsed by the Tauri project.

Licensed under MIT OR Apache-2.0 — see LICENSE-MIT and LICENSE-APACHE.
