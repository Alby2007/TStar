#!/usr/bin/env node
/**
 * pylair CLI. `codegen` is implemented (M2, docs/codegen.md);
 * dev/build/doctor arrive in M4.
 */

import { spawnSync } from "node:child_process";
import { writeFileSync, readFileSync, existsSync } from "node:fs";
import { emit } from "./codegen.js";
import { loadManifest } from "./manifest.js";

const COMMANDS = ["dev", "build", "doctor", "codegen"] as const;

function usage(): void {
  console.log(`pylair — Pylair CLI

Usage: pylair <command> [options]

Commands:
  dev       Run the app with supervised Python + hot reload (M4)
  build     Produce a signed installer with bundled Python (M4)
  doctor    Diagnose environment and app config (M4)
  codegen   Generate typed TypeScript stubs from @expose signatures

codegen options:
  --module, -m <name>   Python module exposing @expose functions (required)
  --out, -o <file>      Output file (default: src/api.generated.ts)
  --python <path>       Interpreter to inspect the module
                        (default: $PYTHON, then python3)
  --check               Exit 1 if the output is stale; don't write
`);
}

function argValue(argv: string[], ...flags: string[]): string | undefined {
  for (const f of flags) {
    const i = argv.indexOf(f);
    if (i !== -1) return argv[i + 1];
  }
  return undefined;
}

function findPython(): string {
  if (process.env.PYTHON) return process.env.PYTHON;
  for (const cand of ["python3", "python"]) {
    const res = spawnProbe(cand);
    if (res) return cand;
  }
  return "python3"; // loadManifest will produce the friendly ENOENT error
}

function spawnProbe(cmd: string): boolean {
  const { status, error } = spawnSync(cmd, ["-c", "import sys;sys.exit(0)"]);
  return !error && status === 0;
}

function codegen(argv: string[]): number {
  const module = argValue(argv, "--module", "-m");
  if (!module) {
    console.error("pylair codegen: --module is required");
    return 2;
  }
  const out = argValue(argv, "--out", "-o") ?? "src/api.generated.ts";
  const python = argValue(argv, "--python") ?? findPython();
  const check = argv.includes("--check");

  const manifest = loadManifest({ python, module });
  const ts = emit(manifest, module);

  if (check) {
    const stale = !existsSync(out) || readFileSync(out, "utf-8") !== ts;
    if (stale) {
      console.error(`pylair codegen: ${out} is stale — re-run codegen`);
      return 1;
    }
    return 0;
  }
  writeFileSync(out, ts);
  const n = manifest.functions?.length ?? 0;
  console.log(`pylair codegen: wrote ${out} (${n} functions)`);
  return 0;
}

function main(argv: string[]): number {
  const [command, ...rest] = argv;
  if (!command || command === "--help" || command === "-h") {
    usage();
    return command ? 0 : 1;
  }
  if (command === "--version" || command === "-v") {
    console.log("0.0.0");
    return 0;
  }
  if (command === "codegen") {
    try {
      return codegen(rest);
    } catch (e) {
      console.error(`pylair codegen: ${(e as Error).message}`);
      return 1;
    }
  }
  console.error(`pylair: unknown command '${command}'. Run 'pylair --help'.`);
  return 1;
}

process.exit(main(process.argv.slice(2)));
