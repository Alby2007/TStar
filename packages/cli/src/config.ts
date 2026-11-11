/** App config for `pylair build` — one file, [tool.pylair] in pyproject.toml.
 * App identity (productName/identifier/version) is derived from
 * src-tauri/tauri.conf.json — Tauri owns it; we don't duplicate it.
 */

import { spawnSync } from "node:child_process";
import { existsSync, readFileSync } from "node:fs";
import { join } from "node:path";

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
