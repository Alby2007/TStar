"""Protocol v1 message shapes and error codes (docs/protocol-v1.md)."""

from __future__ import annotations

from typing import Any

PROTOCOL_VERSION = 1

# JSON-RPC reserved
PARSE_ERROR = -32700
INVALID_REQUEST = -32600
FUNCTION_NOT_FOUND = -32601
INVALID_PARAMS = -32602
# App-specific (-32000..-32099)
INTERNAL = -32000
TASK_NOT_FOUND = -32001
TASK_CANCELLED = -32010
WORKER_DIED = -32011
RUNTIME_WEDGED = -32012
RUNTIME_UNAVAILABLE = -32030
RUNTIME_CRASHED = -32031
PROTOCOL_VIOLATION = -32032
LIMIT_EXCEEDED = -32040

_NAMES = {
    PARSE_ERROR: "PARSE_ERROR",
    INVALID_REQUEST: "INVALID_REQUEST",
    FUNCTION_NOT_FOUND: "FUNCTION_NOT_FOUND",
    INVALID_PARAMS: "INVALID_PARAMS",
    INTERNAL: "INTERNAL",
    TASK_NOT_FOUND: "TASK_NOT_FOUND",
    TASK_CANCELLED: "TASK_CANCELLED",
    WORKER_DIED: "WORKER_DIED",
    RUNTIME_WEDGED: "RUNTIME_WEDGED",
    RUNTIME_UNAVAILABLE: "RUNTIME_UNAVAILABLE",
    RUNTIME_CRASHED: "RUNTIME_CRASHED",
    PROTOCOL_VIOLATION: "PROTOCOL_VIOLATION",
    LIMIT_EXCEEDED: "LIMIT_EXCEEDED",
}

# Defaults — override via env at the host layer.
INITIAL_STREAM_CREDIT = 64
MAX_STREAM_BUFFER = 8 * 1024 * 1024


def code_name(code: int) -> str:
    return _NAMES.get(code, f"ERROR_{code}")


def request(req_id: int | str, method: str, params: dict | None = None) -> dict:
    return {"jsonrpc": "2.0", "id": req_id, "method": method, "params": params or {}}


def response(req_id: int | str, result: Any) -> dict:
    return {"jsonrpc": "2.0", "id": req_id, "result": result}


def error_response(
    req_id: int | str | None, code: int, message: str, data: dict | None = None
) -> dict:
    err: dict[str, Any] = {"code": code, "message": message}
    if data:
        err["data"] = data
    return {"jsonrpc": "2.0", "id": req_id, "error": err}


def notification(method: str, params: dict) -> dict:
    return {"jsonrpc": "2.0", "method": method, "params": params}


def hello(exposes: list[dict]) -> dict:
    import sys

    return notification(
        "hello",
        {
            "protocol": PROTOCOL_VERSION,
            "runtime_version": _runtime_version(),
            "python_version": sys.version.split()[0],
            "exposes": exposes,
        },
    )


def _runtime_version() -> str:
    try:
        from pylair import __version__
    except Exception:  # noqa: BLE001 - version lookup must never kill startup
        __version__ = "0.0.0"
    return __version__


def is_request(msg: dict) -> bool:
    return "id" in msg and "method" in msg


def is_response(msg: dict) -> bool:
    return "id" in msg and "method" not in msg


def is_notification(msg: dict) -> bool:
    return "method" in msg and "id" not in msg
