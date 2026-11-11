#!/usr/bin/env node
/**
 * tauri-plus CLI entry point. Commands (dev, build, doctor, codegen) arrive
 * in M4; this stub exists so the workspace builds and the bin link resolves.
 */

const COMMANDS = ["dev", "build", "doctor", "codegen"] as const;

function main(argv: string[]): number {
  const [command] = argv;
  if (!command || command === "--help" || command === "-h") {
    console.log(`tauri-plus — Tauri+ CLI

Usage: tauri-plus <command>

Commands:
  ${COMMANDS.join("\n  ")}

These commands are not implemented yet (milestone M4).`);
    return command ? 0 : 1;
  }
  if (command === "--version" || command === "-v") {
    console.log("0.0.0");
    return 0;
  }
  console.error(`tauri-plus: unknown command '${command}'. Run 'tauri-plus --help'.`);
  return 1;
}

process.exit(main(process.argv.slice(2)));
