"""@expose decorator: register callables for the Pylair front end.

Kinds reported in the handshake (protocol §4): sync | async | generator |
isolated. `worker` is reserved for the future warm-worker option
(docs/cancellation.md) — v1 accepts only {"mode": "process"}.
"""

from __future__ import annotations

import functools
import inspect
from collections.abc import Callable
from dataclasses import dataclass, field
from typing import Any, overload

# name -> registered function metadata
_registry: dict[str, ExposedFn] = {}


@dataclass
class ExposedFn:
    fn: Callable[..., Any]
    kind: str  # "sync" | "async" | "generator" | "isolated"
    signature: dict[str, Any] = field(default_factory=dict)


def _kind_of(fn: Callable[..., Any], isolated: bool) -> str:
    if isolated:
        return "isolated"
    if inspect.isasyncgenfunction(fn):
        return "generator"
    if inspect.iscoroutinefunction(fn):
        return "async"
    return "sync"


def _signature_of(fn: Callable[..., Any], worker: dict | None) -> dict[str, Any]:
    from pylair._schema import callable_schema

    sig: dict[str, Any] = callable_schema(fn)
    if worker is not None:
        sig["meta"] = {"worker": worker}
    return sig


@overload
def expose[**P, R](fn: Callable[P, R], /) -> Callable[P, R]: ...


@overload
def expose[**P, R](
    fn: None = None, /, *, isolated: bool = False, worker: dict | None = None
) -> Callable[[Callable[P, R]], Callable[P, R]]: ...


def expose(
    fn: Callable[..., object] | None = None,
    /,
    *,
    isolated: bool = False,
    worker: dict | None = None,
) -> Callable[..., object]:
    """Mark a function as callable from the Pylair front end.

    Usable bare (@expose) or with options (@expose(isolated=True)).
    `isolated` runs the call in a worker subprocess the supervisor can
    terminate on cancel — the only safe cancellation for blocking code.

    `worker` is reserved for the warm-worker option (see
    docs/cancellation.md). v1 accepts only {"mode": "process"}.
    """
    if worker is not None and worker.get("mode") != "process":
        raise ValueError(
            "v1 supports only worker={'mode': 'process'}; 'warm' is not implemented yet"
        )

    def decorator(func: Callable[..., object]) -> Callable[..., object]:
        # The name is the wire contract — a collision in another module
        # would silently rebind it. Re-registering the SAME definition
        # (same module + qualname: a reload or re-import) replaces
        # silently so dev hot reload keeps working.
        existing = _registry.get(func.__name__)
        if existing is not None and (
            existing.fn.__module__ != func.__module__
            or existing.fn.__qualname__ != func.__qualname__
        ):
            raise ValueError(
                f"exposed name {func.__name__!r} is already registered by "
                f"{existing.fn.__module__}.{existing.fn.__qualname__}; cannot "
                f"also register {func.__module__}.{func.__qualname__} — "
                "names must be unique across modules"
            )
        _registry[func.__name__] = ExposedFn(
            fn=func,
            kind=_kind_of(func, isolated),
            signature=_signature_of(func, worker),
        )

        @functools.wraps(func)
        def wrapper(*args: object, **kwargs: object) -> object:
            return func(*args, **kwargs)

        return wrapper

    return decorator if fn is None else decorator(fn)


def exposed_functions() -> dict[str, ExposedFn]:
    """Return the registry of @expose'd functions."""
    return dict(_registry)


def exposes_manifest() -> list[dict]:
    """The `exposes` list sent in the hello handshake."""
    return [
        {
            "name": name,
            "kind": ex.kind,
            "signature": ex.signature,
            "doc": inspect.getdoc(ex.fn),
        }
        for name, ex in sorted(_registry.items())
    ]
