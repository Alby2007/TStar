/** `pylair build` — the M0 recipe, generalized:
 *   fetch standalone CPython → stage site-packages via the bundled
 *   interpreter (native wheels only — no cross-compile) → stage app
 *   source → prune → tauri build → unsigned warning.
 */

import { createHash } from "node:crypto";
import { execFileSync, spawnSync } from "node:child_process";
import {
  cpSync,
  createWriteStream,
  existsSync,
  mkdirSync,
  readdirSync,
  readFileSync,
  rmSync,
  writeFileSync,
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

async function fetchPython(
  resources: string,
  cacheDir: string,
  versionClass: string,
): Promise<string> {
  const ver = PBS_VERSIONS[versionClass];
  if (!ver)
    throw new Error(
      `python-version "${versionClass}" not available — supported: ${Object.keys(PBS_VERSIONS).join(", ")}`,
    );
  const triple = hostTriple();
  const asset = `cpython-${ver}+${PBS_TAG}-${triple}-install_only_stripped.tar.gz`;
  const base = `https://github.com/astral-sh/python-build-standalone/releases/download/${PBS_TAG}`;
  mkdirSync(resources, { recursive: true });
  mkdirSync(cacheDir, { recursive: true });
  const tarball = join(cacheDir, asset);

  if (!existsSync(tarball)) {
    console.log(`fetching ${asset}`);
    await download(`${base}/${asset}`, tarball);
  } else {
    console.log(`using cached ${asset}`);
  }

  // The interpreter ships to end users — verify against the release's
  // published SHA256SUMS before it ever reaches the bundle.
  const sumsTmp = join(cacheDir, "SHA256SUMS");
  if (!existsSync(sumsTmp)) await download(`${base}/SHA256SUMS`, sumsTmp);
  const want = readFileSync(sumsTmp, "utf-8")
    .split("\n")
    .find((l) => l.trim().endsWith(` ${asset}`) || l.trim().endsWith(`*${asset}`))
    ?.trim()
    .split(/\s+/)[0];
  if (!want) throw new Error(`no SHA256 for ${asset} in ${PBS_TAG} SHA256SUMS`);
  const got = createHash("sha256").update(readFileSync(tarball)).digest("hex");
  if (got !== want)
    throw new Error(`SHA256 mismatch for ${asset}: got ${got}, want ${want} — refusing to bundle`);

  const dest = join(resources, "python");
  rmSync(dest, { recursive: true, force: true });
  mkdirSync(dest, { recursive: true });
  execFileSync("tar", ["-xzf", tarball, "-C", dest, "--strip-components=1"], { stdio: "inherit" });
  const py = join(dest, isWin ? "python.exe" : "bin/python3");
  if (!existsSync(py)) throw new Error(`expected interpreter at ${py}`);
  return py;
}

function stageEnv(resources: string, python: string, appDir: string, reqs: string, cacheDir: string) {
  const target = join(resources, "site-packages");
  rmSync(target, { recursive: true, force: true });
  // dev uses `uv sync` (uv.lock) — build must install the SAME locked set,
  // hashed, or dev/prod parity is a fiction. requirements.txt is the
  // fallback for projects that don't lock.
  const lock = join(appDir, "uv.lock");
  let reqsFile = reqs;
  let exported: string | undefined;
  if (existsSync(lock)) {
    exported = join(cacheDir, "requirements.exported.txt");
    execFileSync(
      "uv",
      ["export", "--frozen", "--format", "requirements-txt", "-o", exported],
      { cwd: appDir, stdio: "inherit" },
    );
    reqsFile = exported;
  } else if (!existsSync(reqs)) {
    console.log(`no uv.lock and no ${reqs} — bundling stdlib only`);
    return;
  } else {
    console.warn(
      "warning: no uv.lock — building from requirements.txt; " +
        "the bundled env may drift from `pylair dev`'s env",
    );
  }
  execFileSync(
    "uv",
    ["pip", "install", "--python", python, "--target", target, "-r", reqsFile],
    { cwd: appDir, stdio: "inherit" },
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

  const cacheDir = join(appDir, ".pylair", "cache");
  const bundled = opts.skipFetch
    ? join(resources, "python", isWin ? "python.exe" : "bin/python3")
    : await fetchPython(resources, cacheDir, cfg.pythonVersion);

  stageEnv(resources, bundled, appDir, join(appDir, cfg.requirements), cacheDir);

  // Stage app source so resources/ mirrors the installed layout.
  const srcDir = join(appDir, cfg.source);
  if (!existsSync(srcDir)) throw new Error(`source dir ${srcDir} missing`);
  rmSync(join(resources, cfg.source), { recursive: true, force: true });
  cpSync(srcDir, join(resources, cfg.source), { recursive: true });

  const res = prune(resources, [...DEFAULT_PRUNE, ...cfg.prune]);
  const mib = (res.bytesFreed / 2 ** 20).toFixed(1);
  console.log(`pruned ${res.removed} paths (${mib} MiB) from staged resources`);

  // Over-pruning must fail at build time, not on the user's machine:
  // import the app module with the staged tree exactly as the bundle sees it.
  const sep = isWin ? ";" : ":";
  const check = spawnSync(
    bundled,
    ["-c", `import ${cfg.appModule}; print("staged import ok")`],
    {
      encoding: "utf-8",
      env: {
        ...process.env,
        PYTHONPATH: `${join(resources, cfg.source)}${sep}${join(resources, "site-packages")}`,
        PYTHONNOUSERSITE: "1",
      },
    },
  );
  if (check.status !== 0)
    throw new Error(
      `staged bundle failed the import check (${cfg.appModule}) — ` +
        `pruning or missing deps? ${check.stderr?.trim()}`,
    );

  // ── tauri build ────────────────────────────────────────────────────
  // NOTE: TAURI_SIGNING_PRIVATE_KEY signs *updater* artifacts, not the app —
  // it must NOT silence the unsigned warning.
  const hasSigning = !!(
    process.env.APPLE_SIGNING_IDENTITY ||
    process.env.APPLE_CERTIFICATE ||
    process.env.WINDOWS_CERTIFICATE_THUMBPRINT
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
