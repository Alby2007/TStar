/** `pylair dev` — supervised dev mode:
 *   uv sync → codegen → watch the python source dir → on change,
 *   re-run codegen and touch the restart trigger (the supervisor
 *   respawns the runtime within ~250ms) → run `tauri dev`.
 */

import { spawn, spawnSync } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, watch, writeFileSync } from "node:fs";
import { platform } from "node:os";
import { join } from "node:path";
import { importRoot, loadConfig, tauriCliEntry } from "./config.js";
import { emit } from "./codegen.js";
import { loadManifest } from "./manifest.js";

const isWin = platform() === "win32";

export function venvPython(appDir: string): string {
  const p = join(
    appDir,
    ".venv",
    isWin ? "Scripts/python.exe" : "bin/python",
  );
  return p;
}

function runCodegen(appDir: string, cfgModule: string, out: string): boolean {
  const python = venvPython(appDir);
  const env = { ...process.env, PYTHONPATH: join(appDir, "backend") };
  try {
    const manifest = loadManifest({ python, module: cfgModule, env });
    const ts = emit(manifest, cfgModule);
    const prev = existsSync(out) ? readFileSync(out, "utf-8") : null;
    if (prev !== ts) {
      writeFileSync(out, ts);
      console.log(`pylair: codegen → ${out} (${manifest.functions?.length ?? 0} functions)`);
      return true;
    }
  } catch (e) {
    console.warn(`pylair: codegen failed (${(e as Error).message}) — keeping last stubs`);
  }
  return false;
}

export async function dev(opts: { appDir: string }): Promise<number> {
  const { appDir } = opts;
  const cfg = loadConfig(appDir);

  // Sync so the venv exists before codegen/dev need the interpreter.
  const sync = spawnSync("uv", ["sync"], { cwd: appDir, stdio: "inherit" });
  if (sync.error)
    throw new Error(`could not run uv: ${sync.error.message} — is uv on PATH?`);
  if (sync.status !== 0)
    throw new Error("uv sync failed — is pyproject.toml valid?");

  const srcDir = join(appDir, cfg.source);
  const stateDir = join(appDir, ".pylair");
  mkdirSync(stateDir, { recursive: true });
  const trigger = join(stateDir, "restart-trigger");
  writeFileSync(trigger, String(Date.now()));

  const stubs = join(appDir, "src", "api.generated.ts");
  mkdirSync(join(appDir, "src"), { recursive: true });
  runCodegen(appDir, cfg.appModule, stubs);

  // Watch python source; debounce bursts (saves, formatter runs).
  let timer: ReturnType<typeof setTimeout> | null = null;
  let changed = false;
  watch(srcDir, { recursive: true }, (_ev, file) => {
    if (!file?.endsWith(".py")) return;
    changed = true;
    if (timer) clearTimeout(timer);
    timer = setTimeout(() => {
      if (!changed) return;
      changed = false;
      writeFileSync(trigger, String(Date.now()));
      runCodegen(appDir, cfg.appModule, stubs);
    }, 150);
  });

  const env = {
    ...process.env,
    PYLAIR_APP_MODULE: cfg.appModule,
    PYLAIR_PYTHON: venvPython(appDir),
    PYLAIR_PYTHONPATH: importRoot(appDir, cfg.source),
    PYLAIR_RESTART_TRIGGER: trigger,
    PYLAIR_DEV: "1",
  };
  // tauri.conf.json maps resources/* paths that only exist after
  // `pylair build` stages them; `tauri dev` validates them eagerly and
  // fails the build on any missing one. Create empty dirs for dev —
  // `pylair build` replaces them with real staging anyway.
  try {
    const tconf = JSON.parse(
      readFileSync(join(appDir, "src-tauri", "tauri.conf.json"), "utf-8"),
    );
    const res = tconf?.bundle?.resources;
    const keys: string[] = Array.isArray(res) ? res : Object.keys(res ?? {});
    for (const rel of keys) {
      const p = join(appDir, "src-tauri", rel);
      if (!existsSync(p)) mkdirSync(p, { recursive: true });
    }
  } catch {
    // Conf unreadable — `tauri dev` will surface the real error itself.
  }

  console.log("pylair: watching python source; starting tauri dev");
  // Run the app's installed tauri CLI via node — no npx/npx.cmd shell
  // shim (which also trips Node's DEP0190 shell+args warning on Windows).
  const child = spawn(process.execPath, [tauriCliEntry(appDir), "dev"], {
    cwd: appDir,
    stdio: "inherit",
    env,
  });
  return new Promise((resolve) => {
    child.on("exit", (code) => resolve(code ?? 1));
    for (const sig of ["SIGINT", "SIGTERM"] as const)
      process.on(sig, () => child.kill(sig));
  });
}
