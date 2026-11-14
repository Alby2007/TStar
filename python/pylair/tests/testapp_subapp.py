"""App module whose only exposed function lives in a submodule it imports.

`import testapp_pkg.sub` registers sub_ping via @expose at import time,
but the name is NOT re-exported here — a worker must resolve it through
the registry, not getattr on this module.
"""

import testapp_pkg.sub  # noqa: F401 — import side effect: registers sub_ping
