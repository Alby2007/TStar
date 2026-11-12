/** App config for `pylair build` — one file, [tool.pylair] in pyproject.toml.
 * App identity (productName/identifier/version) is derived from
 * src-tauri/tauri.conf.json — Tauri owns it; we don't duplicate it.
 */

import { spawnSync } from "node:child_process";
import { existsSync, readFileSync } from "node:fs";
import { createRequire } from "node:module";
import { dirname, join } from "node:path";

export interface PylairConfig {
  /** Python module holding the @expose functions (e.g. "app.backend"). */
  appModule: string;
  /** Directory staged as the app backend source (default "backend"). */
  source: string;
  /** Locked requirements file (default "requirements.txt"). */
  requirements: string;
  /** Bundled interpreter version class (default "3.12"). */
  pythonVersion: string;
  /** Extra prune globs on top of the defaults. */
  prune: string[];
  /** Bundle target override (default: per-OS sensible set). */
  bundles?: string;
}

export interface TauriConfig {
  productName: string;
  identifier: string;
  version: string;
}

const KEYMAP: Record<string, keyof PylairConfig> = {
  "app-module": "appModule",
  source: "source",
  requirements: "requirements",
  "python-version": "pythonVersion",
  prune: "prune",
  bundles: "bundles",
};

/** Interpreter for parsing [tool.pylair]: the app venv when `uv sync` ran
 * (guaranteed >=3.12 by the template), else a system probe. tomllib needs
 * >=3.11 — Ubuntu 22.04's python3 is 3.10, and user machines may have no
 * python3 at all, so "python3 exists and is new enough" is never safe. */
export function appPython(appDir: string): string {
  const venv = join(
    appDir,
    ".venv",
    process.platform === "win32" ? "Scripts/python.exe" : "bin/python",
  );
  if (existsSync(venv)) return venv;
  for (const c of ["python3", "python"]) {
    if (spawnSync(c, ["--version"]).status === 0) return c;
  }
  throw new Error(`no Python interpreter for ${appDir} — run \`uv sync\` first`);
}

/** Parse [tool.pylair] via the interpreter's own tomllib — no npm TOML dep. */
export function loadConfig(appDir: string, python: string): PylairConfig {
  const pyproject = join(appDir, "pyproject.toml");
  if (!existsSync(pyproject))
    throw new Error(`no pyproject.toml in ${appDir} — is this a pylair app?`);

  const res = spawnSync(
    python,
    [
      "-c",
      "import json,sys,tomllib;print(json.dumps(tomllib.load(open(sys.argv[1],'rb')).get('tool',{}).get('pylair',{})))",
      pyproject,
    ],
    { encoding: "utf-8" },
  );
  if (res.status !== 0)
    throw new Error(
      `could not parse ${pyproject} with ${python} (needs >=3.11 for tomllib): ${res.stderr.trim()}`,
    );
  const raw = JSON.parse(res.stdout) as Record<string, unknown>;

  const cfg: PylairConfig = {
    appModule: String(raw["app-module"] ?? ""),
    source: String(raw.source ?? "backend"),
    requirements: String(raw.requirements ?? "requirements.txt"),
    pythonVersion: String(raw["python-version"] ?? "3.12"),
    prune: Array.isArray(raw.prune) ? raw.prune.map(String) : [],
    bundles: raw.bundles ? String(raw.bundles) : undefined,
  };
  if (!cfg.appModule)
    throw new Error(
      `[tool.pylair] app-module is required in ${pyproject} ` +
        `(the module holding your @expose functions)`,
    );
  return cfg;
}

export function loadTauriConfig(appDir: string): TauriConfig {
  const p = join(appDir, "src-tauri", "tauri.conf.json");
  if (!existsSync(p))
    throw new Error(
      `no src-tauri/tauri.conf.json in ${appDir} — run this inside a pylair app`,
    );
  const raw = JSON.parse(readFileSync(p, "utf-8"));
  return {
    productName: raw.productName ?? "app",
    identifier: raw.identifier ?? "com.example.app",
    version: raw.version ?? "0.1.0",
  };
}

/**
 * The directory that must be on PYTHONPATH for `cfg.appModule` to import:
 * the PARENT of the source dir ("backend.app" lives at <root>/backend/app.py).
 * Single definition shared by dev, the build import check, and (mirrored in
 * Rust) the release spawn spec.
 */
export function importRoot(base: string, source: string): string {
  return join(base, dirname(source));
}

/** Absolute path of the app's installed Tauri CLI entry (JS), so it can be run
 * with `node` directly — no `npx`/`npx.cmd` shell shim, same on every OS. */
export function tauriCliEntry(appDir: string): string {
  try {
    return createRequire(join(appDir, "package.json")).resolve("@tauri-apps/cli/tauri.js");
  } catch {
    throw new Error("@tauri-apps/cli is not installed in the app (run `pnpm install`).");
  }
}
