#!/usr/bin/env node
/**
 * Stage the bundled resources: install the locked requirements into
 * src-tauri/resources/site-packages using the *bundled* interpreter (so
 * wheels always match the target), and sync backend/ into the staged tree
 * so `src-tauri/resources/` mirrors the installed app exactly.
 *
 * Requires `uv` on PATH. Run fetch-python first.
 */
import { cpSync, existsSync, rmSync } from "node:fs";
import { execFileSync } from "node:child_process";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { platform } from "node:os";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const resources = join(root, "src-tauri", "resources");
const isWin = platform() === "win32";
const python = join(resources, "python", isWin ? "python.exe" : join("bin", "python3"));
const siteTarget = join(resources, "site-packages");
const backendSrc = join(root, "backend");
const backendTarget = join(resources, "backend");
const reqs = join(backendSrc, "requirements.txt");

for (const p of [python, reqs]) {
  if (!existsSync(p)) throw new Error(`missing ${p} — run pnpm fetch:python and pnpm compile:reqs first`);
}

if (existsSync(siteTarget)) rmSync(siteTarget, { recursive: true });
execFileSync(
  "uv",
  ["pip", "install", "--python", python, "--target", siteTarget, "-r", reqs],
  { stdio: "inherit" }
);

rmSync(backendTarget, { recursive: true, force: true });
cpSync(backendSrc, backendTarget, { recursive: true });

console.log(`ok: site-packages + backend staged under ${resources}`);
