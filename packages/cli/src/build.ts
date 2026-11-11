/** `pylair build` — the M0 recipe, generalized:
 *   fetch standalone CPython → stage site-packages via the bundled
 *   interpreter (native wheels only — no cross-compile) → stage app
 *   source → prune → tauri build → unsigned warning.
 */

import { execFileSync, spawnSync } from "node:child_process";
import {
  cpSync,
  createWriteStream,
  existsSync,
  mkdirSync,
  readdirSync,
  rmSync,
} from "node:fs";
import { get } from "node:https";
import { arch, platform } from "node:os";
import { dirname, join } from "node:path";
import { loadConfig, loadTauriConfig } from "./config.js";
import { DEFAULT_PRUNE, prune } from "./prune.js";

const PBS_TAG = "20251120";
const PBS_VERSIONS: Record<string, string> = { "3.12": "3.12.12", "3.13": "3.13.9" };
const HOST_TRIPLES: Record<string, string> = {
  "darwin:arm64": "aarch64-apple-darwin",
  "darwin:x64": "x86_64-apple-darwin",
  "win32:x64": "x86_64-pc-windows-msvc",
  "linux:x64": "x86_64-unknown-linux-gnu",
  "linux:arm64": "aarch64-unknown-linux-gnu",
};

const isWin = platform() === "win32";

export function hostTriple(): string {
  const t = HOST_TRIPLES[`${platform()}:${arch()}`];
  if (!t) throw new Error(`no standalone CPython build for ${platform()}/${arch()}`);
  return t;
}

function download(url: string, to: string): Promise<void> {
  return new Promise((res, rej) => {
    const follow = (u: string) =>
      get(u, (r) => {
        if (r.statusCode! >= 300 && r.statusCode! < 400 && r.headers.location)
          return follow(r.headers.location);
        if (r.statusCode !== 200) return rej(new Error(`GET ${u} -> ${r.statusCode}`));
        const out = createWriteStream(to);
        r.pipe(out);
        out.on("finish", () => out.close(() => res()));
        out.on("error", rej);
      }).on("error", rej);
    follow(url);
  });
}

async function fetchPython(resources: string, versionClass: string): Promise<string> {
  const ver = PBS_VERSIONS[versionClass];
  if (!ver)
    throw new Error(
      `python-version "${versionClass}" not available — supported: ${Object.keys(PBS_VERSIONS).join(", ")}`,
    );
  const triple = hostTriple();
  const asset = `cpython-${ver}+${PBS_TAG}-${triple}-install_only_stripped.tar.gz`;
  const url = `https://github.com/astral-sh/python-build-standalone/releases/download/${PBS_TAG}/${asset}`;
  const dest = join(resources, "python");
  const tmp = join(resources, ".python.tar.gz");
  mkdirSync(resources, { recursive: true });
  rmSync(dest, { recursive: true, force: true });
  mkdirSync(dest, { recursive: true });
  console.log(`fetching ${asset}`);
  await download(url, tmp);
  execFileSync("tar", ["-xzf", tmp, "-C", dest, "--strip-components=1"], { stdio: "inherit" });
  rmSync(tmp);
  const py = join(dest, isWin ? "python.exe" : "bin/python3");
  if (!existsSync(py)) throw new Error(`expected interpreter at ${py}`);
  return py;
}

function stageEnv(resources: string, python: string, reqs: string) {
  const target = join(resources, "site-packages");
  rmSync(target, { recursive: true, force: true });
  execFileSync(
    "uv",
    ["pip", "install", "--python", python, "--target", target, "-r", reqs],
    { stdio: "inherit" },
  );
}

function run(cmd: string, args: string[], cwd: string) {
  const r = spawnSync(cmd, args, { cwd, stdio: "inherit" });
  if (r.status !== 0) throw new Error(`${cmd} ${args.join(" ")} exited ${r.status}`);
}

export interface BuildOptions {
  appDir: string;
  /** Extra tauri-cli args (e.g. --bundles). */
  tauriArgs?: string[];
  /** Skip the interpreter fetch (reuse staged resources/python). */
  skipFetch?: boolean;
}

export async function build(opts: BuildOptions): Promise<void> {
  const { appDir } = opts;
  const tauriDir = join(appDir, "src-tauri");
  const resources = join(tauriDir, "resources");
  const tconf = loadTauriConfig(appDir);

  const py = spawnSync("python3", ["--version"], { encoding: "utf-8" });
  const python = py.status === 0 ? "python3" : "python";
  const cfg = loadConfig(appDir, python);

  const targetTriple = hostTriple(); // cross-compile refused by construction
  console.log(
    `building ${tconf.productName} ${tconf.version} for ${platform()}/${arch()} (${targetTriple})`,
  );
  console.log("note: pylair builds native bundles only — run this on each target OS");

  const bundled = opts.skipFetch
    ? join(resources, "python", isWin ? "python.exe" : "bin/python3")
    : await fetchPython(resources, cfg.pythonVersion);

  const reqs = join(appDir, cfg.requirements);
  if (existsSync(reqs)) stageEnv(resources, bundled, reqs);
  else console.log(`no ${cfg.requirements} — app will bundle stdlib only`);

  // Stage app source so resources/ mirrors the installed layout.
  const srcDir = join(appDir, cfg.source);
  if (!existsSync(srcDir)) throw new Error(`source dir ${srcDir} missing`);
  rmSync(join(resources, cfg.source), { recursive: true, force: true });
  cpSync(srcDir, join(resources, cfg.source), { recursive: true });

  const res = prune(resources, [...DEFAULT_PRUNE, ...cfg.prune]);
  const mib = (res.bytesFreed / 2 ** 20).toFixed(1);
  console.log(`pruned ${res.removed} paths (${mib} MiB) from staged resources`);

  // ── tauri build ────────────────────────────────────────────────────
  const hasSigning = !!(
    process.env.APPLE_SIGNING_IDENTITY ||
    process.env.TAURI_SIGNING_PRIVATE_KEY ||
    process.env.WINDOWS_CERTIFICATE_THUMBPRINT ||
    process.env.SMAC_CODE_SIGNING_IDENTITY
  );
  run("npx", ["tauri", "build", ...(opts.tauriArgs ?? [])], appDir);
  if (!hasSigning) {
    console.warn(`
╔══════════════════════════════════════════════════════════════════╗
║  UNSIGNED BUILD — this bundle is not code-signed or notarized.     ║
║  macOS Gatekeeper and Windows SmartScreen will warn users.         ║
║  To sign: set APPLE_SIGNING_IDENTITY / TAURI_SIGNING_PRIVATE_KEY / ║
║  WINDOWS_CERTIFICATE_THUMBPRINT (see docs/build.md).               ║
╚══════════════════════════════════════════════════════════════════╝`);
  }
}
