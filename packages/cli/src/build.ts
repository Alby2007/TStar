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
import { arch, platform, tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { importRoot, loadConfig, loadTauriConfig, tauriCliEntry } from "./config.js";
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

/** Expected SHA256 per asset, committed in-repo — a hash fetched from the
 * same release as the tarball catches corruption but not a tampered
 * release. Bump together with PBS_TAG/PBS_VERSIONS; verify each value
 * against upstream SHA256SUMS before committing.
 */
const PBS_SHA256: Record<string, string> = {
  "3.12.12-aarch64-apple-darwin": "9955ac87eff0d561e64920dc46fd8625912d2c2be4a53c66ad80125d6d9fbdb7",
  "3.12.12-aarch64-unknown-linux-gnu": "4c4594cb38ddf6769f11f86818a4c6617652f4994e5a962eb4f8cf0ec977a1be",
  "3.12.12-x86_64-apple-darwin": "1b4d8eec597ff69a3b67946f30622a6c25c6612e6d15aa431b864ebff66ac8a1",
  "3.12.12-x86_64-pc-windows-msvc": "70ecfaf0bfc58298db11d6389be92c29ec4ef8cd93ebb6a7e54b41d9fc056b10",
  "3.12.12-x86_64-unknown-linux-gnu": "8d2cd1393c1aec9d701cebeccb83d1519951f71d3ecb42d5e70c70af4db9be34",
  "3.13.9-aarch64-apple-darwin": "29d38bd5fbe267b37cecac990015c662796edcf51c2f5e321d59ec06a1850816",
  "3.13.9-aarch64-unknown-linux-gnu": "fdc5d80488b64fcef3b99f297618f68e5374c13d89e888a66abeff4967afd308",
  "3.13.9-x86_64-apple-darwin": "9da51afb00480a8ccd4909bd302c80d9c104d633c5e865cf37dd5e587185e125",
  "3.13.9-x86_64-pc-windows-msvc": "f4c22b31ddbf8d7824cbcba2d8707621c2c8fab1fb6d2c1810c2bb0304d8e9a8",
  "3.13.9-x86_64-unknown-linux-gnu": "1bc9a94ab2425fb6ae58e45f69cab61647759b57895649701390822d976fa923",
};

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

  // The interpreter ships to end users — verify against hashes committed
  // in-repo (see PBS_SHA256), not a checksum file from the same release.
  const want = PBS_SHA256[`${ver}-${triple}`];
  if (!want) throw new Error(`no committed SHA256 for ${ver}/${triple}`);
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
  if (r.error) throw new Error(`${cmd} ${args.join(" ")} failed to start: ${r.error.message}`);
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
  // The import root is the PARENT of the staged source dir ("backend.app"
  // resolves from resources/, not resources/backend/). Run from an unrelated
  // cwd so the project checkout can't satisfy the import by accident.
  const check = spawnSync(
    bundled,
    ["-c", `import ${cfg.appModule}; print("staged import ok")`],
    {
      encoding: "utf-8",
      cwd: tmpdir(),
      env: {
        ...process.env,
        PYTHONPATH: `${importRoot(resources, cfg.source)}${sep}${join(resources, "site-packages")}`,
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
  run(process.execPath, [tauriCliEntry(appDir), "build", ...(opts.tauriArgs ?? [])], appDir);
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
