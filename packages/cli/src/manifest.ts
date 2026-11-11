/** Load the codegen manifest by running `python -m pylair.codegen`. */

import { spawnSync } from "node:child_process";
import type { Manifest } from "./codegen.js";

export interface ManifestOptions {
  /** Interpreter to run; resolution order handled by the caller. */
  python: string;
  /** Module the interpreter must import (its venv/PYTHONPATH supplies it). */
  module: string;
  /** Extra env (e.g. PYTHONPATH) merged over process.env. */
  env?: Record<string, string>;
  /** Working directory for the interpreter (app dir in practice). */
  cwd?: string;
}

export function loadManifest(opts: ManifestOptions): Manifest {
  const res = spawnSync(
    opts.python,
    ["-m", "pylair.codegen", opts.module],
    {
      encoding: "utf-8",
      cwd: opts.cwd,
      env: { ...process.env, ...opts.env },
      maxBuffer: 64 * 1024 * 1024,
    },
  );
  if (res.error) {
    const e = res.error as NodeJS.ErrnoException;
    if (e.code === "ENOENT")
      throw new Error(
        `python interpreter not found: ${opts.python} ` +
          `(use --python or set PYTHON)`,
      );
    throw res.error;
  }
  if (res.status !== 0) {
    let detail = res.stdout?.trim() || res.stderr?.trim() || "";
    try {
      const parsed = JSON.parse(res.stdout) as { error?: string };
      if (parsed.error) detail = parsed.error;
    } catch {
      /* not JSON — use raw output */
    }
    throw new Error(
      `codegen manifest failed for module '${opts.module}': ${detail}`,
    );
  }
  try {
    return JSON.parse(res.stdout) as Manifest;
  } catch {
    throw new Error(
      `pylair.codegen printed no manifest for '${opts.module}' ` +
        `(stdout: ${res.stdout.slice(0, 200)})`,
    );
  }
}
