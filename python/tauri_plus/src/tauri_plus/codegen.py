"""Codegen manifest: `python -m tauri_plus.codegen <module>`.

Imports the app module (decorators fill the @expose registry) and prints
the manifest JSON consumed by `tauri-plus codegen` (docs/codegen.md).
Runs as a plain script — stdout hygiene is the runtime's concern, not
this entry point's.
"""

from __future__ import annotations

import argparse
import importlib
import sys

from tauri_plus import _json
from tauri_plus._expose import exposes_manifest


def manifest(module_name: str) -> dict:
    importlib.import_module(module_name)
    return {
        "version": 1,
        "functions": [
            {
                "name": e["name"],
                "kind": e["kind"],
                "doc": e.get("doc"),
                "params": e["signature"]["params"],
                "returns": e["signature"]["returns"],
            }
            for e in exposes_manifest()
        ],
    }


def main() -> int:
    ap = argparse.ArgumentParser(prog="python -m tauri_plus.codegen")
    ap.add_argument("module", help="application module to inspect")
    args = ap.parse_args()
    try:
        print(_json.dumps(manifest(args.module)))
    except BaseException as e:  # noqa: BLE001 - report import failures cleanly
        print(
            _json.dumps(
                {
                    "version": 1,
                    "error": f"{type(e).__name__}: {e}",
                }
            )
        )
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
