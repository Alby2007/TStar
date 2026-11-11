"""Pylair Python runtime.

Stub until M2 (typed bridge). Will provide the @expose decorator, the
JSON-RPC protocol server, the task runner, cancellation handling and
schema export. See PROJECT_BRIEF.md sections 4-5.
"""

from pylair._expose import expose

__all__ = ["expose"]
__version__ = "0.0.0"
