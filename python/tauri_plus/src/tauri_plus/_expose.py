"""@expose decorator stub.

Final semantics (M2): register a function as callable from the TypeScript
side, record its type signature for schema export, and mark functions with
isolated=True for process-pool execution (blocking/CPU-bound work).
"""

from __future__ import annotations

import functools
from collections.abc import Callable
from typing import overload

# Registry of exposed functions. Real implementation records type signatures
# for `tauri-plus codegen` schema export.
_registry: dict[str, Callable[..., object]] = {}


@overload
def expose[**P, R](fn: Callable[P, R], /) -> Callable[P, R]: ...


@overload
def expose[**P, R](
    fn: None = None, /, *, isolated: bool = False
) -> Callable[[Callable[P, R]], Callable[P, R]]: ...


def expose(
    fn: Callable[..., object] | None = None,
    /,
    *,
    isolated: bool = False,
) -> Callable[..., object]:
    """Mark a function as callable from the Tauri+ front end.

    Usable bare (@expose) or with options (@expose(isolated=True)).
    `isolated` requests execution in a worker subprocess so the supervisor
    can terminate blocking work on cancellation.
    """

    def decorator(func: Callable[..., object]) -> Callable[..., object]:
        _registry[func.__name__] = func

        @functools.wraps(func)
        def wrapper(*args: object, **kwargs: object) -> object:
            return func(*args, **kwargs)

        return wrapper

    return decorator if fn is None else decorator(fn)


def exposed_functions() -> dict[str, Callable[..., object]]:
    """Return the registry of @expose'd functions."""
    return dict(_registry)
