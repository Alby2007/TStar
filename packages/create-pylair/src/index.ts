#!/usr/bin/env node
/** create-pylair — scaffold a new Pylair app.
 *
 *   npx create-pylair my-app [--identifier com.example.app] [--local /path/to/pylair-repo]
 *
 * --local points all three pylair deps (cargo crate, pypi package, npm
 * package) at a local checkout — used by this repo's own acceptance test
 * until the packages are published.
 */

import { cpSync, existsSync, mkdirSync, readdirSync, readFileSync, statSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const TEMPLATES = resolve(fileURLToPath(import.meta.url), "../../templates");
const BINARY = new Set([".png", ".ico", ".icns"]);

function argValue(argv: string[], flag: string): string | undefined {
  const i = argv.indexOf(flag);
  return i === -1 ? undefined : argv[i + 1];
}

function fail(msg: string): never {
  console.error(`create-pylair: ${msg}`);
  process.exit(1);
}

export function substitutions(name: string, identifier: string, local?: string): Record<string, string> {
  const snake = name.replace(/-/g, "_").replace(/\W/g, "_");
  const subs: Record<string, string> = {
    __APP_NAME__: name,
    __APP_ID__: identifier,
    __APP_SNAKE__: snake,
  };
  if (local) {
    const root = resolve(local);
    const unixPath = (p: string) => p.replace(/\\/g, "/");
    subs.__PYLAIR_PY__ = `pylair @ file://${unixPath(join(root, "python/pylair"))}`;
    subs.__PYLAIR_JS_PKG__ = "@pylair/client";
    subs.__PYLAIR_JS__ = `file:${unixPath(join(root, "packages/client"))}`;
    subs.__PYLAIR_RS__ = `tauri-plugin-pylair = { path = "${unixPath(join(root, "crates/tauri-plugin-pylair"))}" }`;
    subs.__PYLAIR_CLI__ = `file:${unixPath(join(root, "packages/cli"))}`;
  } else {
    subs.__PYLAIR_PY__ = "pylair>=0.1";
    subs.__PYLAIR_JS_PKG__ = "@pylair/client";
    subs.__PYLAIR_JS__ = "^0.1.0";
    subs.__PYLAIR_CLI__ = "^0.1.0";
    subs.__PYLAIR_RS__ = `tauri-plugin-pylair = "0.1"`;
  }
  return subs;
}

function fill(text: string, subs: Record<string, string>): string {
  let out = text;
  for (const [k, v] of Object.entries(subs)) out = out.split(k).join(v);
  return out;
}

function copyTree(src: string, dst: string, subs: Record<string, string>) {
  mkdirSync(dst, { recursive: true });
  for (const e of readdirSync(src, { withFileTypes: true })) {
    const s = join(src, e.name);
    const isTmpl = e.name.endsWith(".tmpl");
    const d = join(dst, isTmpl ? e.name.slice(0, -5) : e.name);
    if (e.isDirectory()) {
      copyTree(s, d, subs);
    } else if (isTmpl || !BINARY.has(e.name.slice(e.name.lastIndexOf(".")))) {
      writeFileSync(d, fill(readFileSync(s, "utf-8"), subs));
    } else {
      cpSync(s, d);
    }
  }
}

function main(argv: string[]): number {
  const positional = argv.filter((a) => !a.startsWith("--") && argv[argv.indexOf(a) - 1] !== "--local" && argv[argv.indexOf(a) - 1] !== "--identifier" && argv[argv.indexOf(a) - 1] !== "--template");
  const name = positional[0];
  if (!name || name === "create-pylair") {
    console.log("usage: create-pylair <app-name> [--identifier com.x.y] [--local /repo] [--template default]");
    return name ? 1 : 0;
  }
  if (!/^[a-z][a-z0-9-]*$/.test(name))
    fail(`invalid app name '${name}' — use lowercase letters, digits, dashes`);

  const identifier = argValue(argv, "--identifier") ?? `com.example.${name.replace(/-/g, "")}`;
  const local = argValue(argv, "--local");
  const template = argValue(argv, "--template") ?? "default";
  const templateDir = join(TEMPLATES, template);
  if (!existsSync(templateDir)) fail(`no template '${template}' at ${templateDir}`);

  const dest = resolve(process.cwd(), name);
  if (existsSync(dest) && readdirSync(dest).length) fail(`${dest} exists and is not empty`);

  copyTree(templateDir, dest, substitutions(name, identifier, local));
  if (!statSync(join(dest, "src-tauri/tauri.conf.json")).isFile())
    fail("template produced no tauri.conf.json — internal error");

  console.log(`created ${name}/

next:
  cd ${name}
  pnpm install        # or npm install
  uv sync             # python env (https://docs.astral.sh/uv/)
  pylair dev          # supervised dev: python hot reload + vite
  pylair build        # native installer with bundled python
  pylair doctor       # diagnose environment any time
`);
  return 0;
}

process.exit(main(process.argv.slice(2)));
