#!/usr/bin/env node
/**
 * Fetch a python-build-standalone interpreter for a target triple and unpack
 * it into src-tauri/resources/python so `tauri build` bundles it.
 *
 * Usage: node scripts/fetch-python.mjs [--target <pbs-triple>]
 *
 * Pinned release: see PBS_TAG/PBS_VERSION below (bump together, deliberately).
 */
import { createHash } from "node:crypto";
import { createWriteStream, existsSync, mkdirSync, readdirSync, rmSync, statSync } from "node:fs";
import { get } from "node:https";
import { dirname, join, resolve } from "node:path";
import { execFileSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import { platform, arch } from "node:os";

const PBS_TAG = "20261110";
const PBS_VERSION = "3.12.16";
const BASE = `https://github.com/astral-sh/python-build-standalone/releases/download/${PBS_TAG}`;

const HOST_TRIPLES = {
  "darwin:arm64": "aarch64-apple-darwin",
  "darwin:x64": "x86_64-apple-darwin",
  "win32:x64": "x86_64-pc-windows-msvc",
  "linux:x64": "x86_64-unknown-linux-gnu",
  "linux:arm64": "aarch64-unknown-linux-gnu",
};

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const dest = join(root, "src-tauri", "resources", "python");

function hostTriple() {
  const t = HOST_TRIPLES[`${platform()}:${arch()}`];
  if (!t) throw new Error(`no PBS triple known for ${platform()}/${arch()}`);
  return t;
}

function download(url, to) {
  return new Promise((resolvePromise, reject) => {
    const follow = (u) => {
      get(u, (res) => {
        if (res.statusCode >= 300 && res.statusCode < 400 && res.headers.location) {
          return follow(res.headers.location);
        }
        if (res.statusCode !== 200) {
          return reject(new Error(`GET ${u} -> ${res.statusCode}`));
        }
        const out = createWriteStream(to);
        res.pipe(out);
        out.on("finish", () => out.close(resolvePromise));
        out.on("error", reject);
      }).on("error", reject);
    };
    follow(url);
  });
}

function dirSizeBytes(dir) {
  let total = 0;
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const p = join(dir, entry.name);
    total += entry.isDirectory() ? dirSizeBytes(p) : statSync(p).size;
  }
  return total;
}

const ti = process.argv.indexOf("--target");
const triple = ti === -1 ? hostTriple() : process.argv[ti + 1];
if (!triple) throw new Error("--target requires a value");

const asset = `cpython-${PBS_VERSION}+${PBS_TAG}-${triple}-install_only_stripped.tar.gz`;
const url = `${BASE}/${asset}`;
const tmpTar = join(root, "src-tauri", "resources", ".python.tar.gz");

mkdirSync(dirname(tmpTar), { recursive: true });
if (existsSync(dest)) rmSync(dest, { recursive: true });
mkdirSync(dest, { recursive: true });

console.log(`downloading ${asset}`);
await download(url, tmpTar);

console.log("extracting to src-tauri/resources/python");
// bsdtar (Windows) and bsdtar/gtar (mac/linux) all handle .tar.gz + --strip-components
execFileSync("tar", ["-xzf", tmpTar, "-C", dest, "--strip-components=1"], { stdio: "inherit" });
rmSync(tmpTar);

const pyBin = triple.includes("windows") ? join(dest, "python.exe") : join(dest, "bin", "python3");
if (!existsSync(pyBin)) throw new Error(`expected interpreter at ${pyBin} after extract`);

const mib = (dirSizeBytes(dest) / 2 ** 20).toFixed(1);
console.log(`ok: ${pyBin}  (${mib} MiB on disk)`);
