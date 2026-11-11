"""Runtime entry point: `python -m pylair <app.module>`.

Order matters: stdout hygiene runs BEFORE importing the user module, so
import-time prints can never touch the frame channel.
"""

from __future__ import annotations

import importlib
import os
import sys


def main() -> int:
    module = sys.argv[1] if len(sys.argv) > 1 else os.environ.get("PYLAIR_APP_MODULE")
    if not module:
        print(
            "usage: python -m pylair <app.module>  (or set PYLAIR_APP_MODULE)",
            file=sys.stderr,
        )
        return 2
    os.environ["PYLAIR_APP_MODULE"] = module

    from pylair._server import run
    from pylair._transport import open_frame_channel

    channel = open_frame_channel()  # hygiene first — before user imports
    importlib.import_module(module)  # registers @expose'd functions
    run(channel)
    return 0


if __name__ == "__main__":
    sys.exit(main())
