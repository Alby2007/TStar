"""Wire JSON for the pylair protocol.

Plain json.dumps can't serialize bytes, dataclass instances, or
datetime — this layer makes the annotated types in docs/codegen.md
actually round-trip:

  bytes/bytearray -> {"$b64": "<base64>"}  (restored by object_hook)
  @dataclass      -> dataclasses.asdict
  datetime/date/time -> ISO 8601 string
  Enum -> its value; set/frozenset -> list

Param coercion (coerce_kwargs) reconstructs annotated types on the call
path: dict -> dataclass, containers recurse. Construction failures raise
InvalidParams (wire INVALID_PARAMS); annotations that don't map to a
reconstructable type pass the wire value through untouched.

NaN/Infinity are rejected at encode time (allow_nan=False) — a bare NaN
token would be a malformed frame to the Rust host, and a model returning
a NaN score must fail one call, never the runtime (protocol §2).
"""

from __future__ import annotations

import base64
import binascii
import dataclasses
import datetime as _dt
import enum
import inspect
import json
import types
import typing
from typing import Any

_B64_TAG = "$b64"

# JS Number.MAX_SAFE_INTEGER — the largest int that survives JSON.parse.
_SAFE_INT = (1 << 53) - 1


def _guard_int(tok: str) -> int:
    v = int(tok)
    if abs(v) > _SAFE_INT:
        raise ValueError(
            f"integer {v} exceeds the JSON safe-integer range (±2^53-1); "
            "return it as a string — $bigint tagging is tracked in docs/ideas.md"
        )
    return v


class InvalidParams(Exception):
    """Incoming params couldn't be coerced by annotation.

    Maps to INVALID_PARAMS on the wire (protocol §8) — the host sent a
    value the annotated signature can't hold (e.g. a dataclass field
    missing), which is a caller error, not an internal failure.
    """


def _default(obj: Any) -> Any:
    if isinstance(obj, (bytes, bytearray)):
        return {_B64_TAG: base64.b64encode(bytes(obj)).decode("ascii")}
    if dataclasses.is_dataclass(obj) and not isinstance(obj, type):
        return dataclasses.asdict(obj)
    if isinstance(obj, (_dt.datetime, _dt.date, _dt.time)):
        return obj.isoformat()
    if isinstance(obj, enum.Enum):
        return obj.value
    if isinstance(obj, (set, frozenset)):
        return sorted(obj, key=repr)
    raise TypeError(f"{type(obj).__name__} is not wire-serializable")


def _object_hook(d: dict) -> Any:
    if set(d) == {_B64_TAG} and isinstance(d[_B64_TAG], str):
        try:
            return base64.b64decode(d[_B64_TAG], validate=True)
        except Exception:  # noqa: BLE001 - invalid tag stays a plain object
            return d
    return d


def dumps(obj: Any) -> str:
    # allow_nan=False: a bare NaN/Infinity token is invalid JSON to the
    # Rust host (serde_json rejects it as a malformed frame) — surface
    # it as a serialization error for this call instead.
    s = json.dumps(obj, default=_default, separators=(",", ":"), allow_nan=False)
    # The wire text keeps full int precision; the loss happens when JS
    # JSON.parse reads it back as a double. Re-parse and reject integers
    # JS can't represent exactly — a wrong number is worse than a failed
    # call. Validates the emitted text, so dataclass/enum conversions in
    # _default are covered too. Until $bigint tagging lands (ideas.md).
    json.loads(s, parse_int=_guard_int)
    return s


def loads(raw: str | bytes) -> Any:
    return json.loads(raw, object_hook=_object_hook)


# ---- input validation + coercion: dict -> annotated types ------------------

_SHORT_LIMIT = 80


def _typename(ann: Any) -> str:
    return getattr(ann, "__name__", str(ann))


def _short(value: Any) -> str:
    """repr for error messages — never echo a huge user-supplied value."""
    r = repr(value)
    return r if len(r) <= _SHORT_LIMIT else r[:_SHORT_LIMIT] + "…"


def coerce(annotation: Any, value: Any) -> Any:
    """Validate `value` (wire JSON) against `annotation` and rebuild the
    annotated type. Raises InvalidParams on mismatch — a value that can't
    hold the annotation must never reach the function body. Annotations
    this layer doesn't model pass through unchanged."""
    if (
        annotation is inspect.Parameter.empty
        or annotation is Any
        or annotation is object
    ):
        return value

    origin = typing.get_origin(annotation)
    args = typing.get_args(annotation)

    # Optional[T] / Union[...] — valid if any arm matches.
    if origin in (typing.Union, types.UnionType):
        if value is None and type(None) in args:
            return None
        for arm in args:
            if arm is type(None):
                continue
            try:
                return coerce(arm, value)
            except Exception:  # noqa: BLE001,S112 - try the next union arm
                continue
        raise InvalidParams(f"expected {annotation}, got {_short(value)}")

    # null is only valid for Optional (handled above).
    if value is None:
        raise InvalidParams(f"expected {_typename(annotation)}, got null")

    # Primitives: strict wire types. bool precedes int — isinstance(True, int)
    # is True, and JSON true is never a valid int/float.
    if annotation is bool:
        if isinstance(value, bool):
            return value
        raise InvalidParams(f"expected bool, got {_short(value)}")
    if annotation is int:
        # JSON can't distinguish 3 from 3.0 — whole floats are ints.
        if isinstance(value, int) and not isinstance(value, bool):
            return value
        if isinstance(value, float) and value.is_integer():
            return int(value)
        raise InvalidParams(f"expected int, got {_short(value)}")
    if annotation is float:
        if isinstance(value, (int, float)) and not isinstance(value, bool):
            return float(value)
        raise InvalidParams(f"expected float, got {_short(value)}")
    if annotation is str:
        if isinstance(value, str):
            return value
        raise InvalidParams(f"expected str, got {_short(value)}")

    if inspect.isclass(annotation) and dataclasses.is_dataclass(annotation):
        if not isinstance(value, dict):
            raise InvalidParams(
                f"expected object for {_typename(annotation)}, got {_short(value)}"
            )
        fields = {f.name: f for f in dataclasses.fields(annotation)}
        try:
            # Under `from __future__ import annotations` f.type is a
            # string — resolved hints are required for nested types.
            hints = typing.get_type_hints(annotation)
        except Exception:  # noqa: BLE001 - fall back to raw field types
            hints = {}
        try:
            kwargs = {
                k: coerce(hints.get(k, fields[k].type), v)
                for k, v in value.items()
                if k in fields
            }
            return annotation(**kwargs)
        except InvalidParams:
            raise
        except Exception as e:
            raise InvalidParams(f"{_typename(annotation)}: {e}") from e

    if origin in (list, set, frozenset) or annotation is list:
        if not isinstance(value, list):
            raise InvalidParams(
                f"expected list for {_typename(annotation)}, got {_short(value)}"
            )
        (item_t,) = args or (inspect.Parameter.empty,)
        items = [coerce(item_t, v) for v in value]
        return set(items) if origin in (set, frozenset) else items

    if origin is tuple:
        if not isinstance(value, list):
            raise InvalidParams(f"expected list for tuple, got {_short(value)}")
        if len(args) == 2 and args[1] is Ellipsis:
            return tuple(coerce(args[0], v) for v in value)
        return tuple(coerce(t, v) for t, v in zip(args, value, strict=False))

    if origin is dict or annotation is dict:
        if not isinstance(value, dict):
            raise InvalidParams(f"expected object for dict, got {_short(value)}")
        val_t = args[1] if len(args) == 2 else inspect.Parameter.empty
        return {k: coerce(val_t, v) for k, v in value.items()}

    if annotation in (_dt.datetime, _dt.date, _dt.time):
        if isinstance(value, str):
            try:
                return annotation.fromisoformat(value)
            except ValueError:
                pass
        raise InvalidParams(
            f"expected ISO-8601 {_typename(annotation)}, got {_short(value)}"
        )

    if annotation is bytes:
        if isinstance(value, (bytes, bytearray)):
            return bytes(value)
        if isinstance(value, str):
            # TS may send a bare base64 string for a bytes-annotated param.
            try:
                return base64.b64decode(value, validate=True)
            except (binascii.Error, ValueError):
                raise InvalidParams(
                    f"expected base64 bytes, got {_short(value)}"
                ) from None
        raise InvalidParams(f"expected base64 bytes, got {_short(value)}")

    if inspect.isclass(annotation) and issubclass(annotation, enum.Enum):
        try:
            return annotation(value)
        except ValueError:
            raise InvalidParams(
                f"expected {_typename(annotation)}, got {_short(value)}"
            ) from None

    return value


def coerce_kwargs(fn: Any, input: dict) -> dict:
    """Validate an incoming `input` dict against `fn`'s signature and coerce
    values to the annotated types. Raises InvalidParams for missing required
    params, unexpected keys (unless `**kwargs`), or a value the annotation
    can't hold — the function body never sees bad input."""
    try:
        sig = inspect.signature(fn)
    except (TypeError, ValueError):
        return input
    try:
        hints = typing.get_type_hints(fn)
    except Exception:  # noqa: BLE001 - unresolved refs fall back to raw anns
        hints = {}
    params = sig.parameters

    has_varkw = any(p.kind is inspect.Parameter.VAR_KEYWORD for p in params.values())
    if not has_varkw:
        for k in input:
            if k not in params:
                raise InvalidParams(f"unexpected parameter '{k}'")

    for name, p in params.items():
        if (
            p.default is inspect.Parameter.empty
            and p.kind
            not in (inspect.Parameter.VAR_POSITIONAL, inspect.Parameter.VAR_KEYWORD)
            and name not in input
        ):
            raise InvalidParams(f"missing required parameter '{name}'")

    out = dict(input)
    for name, param in params.items():
        if name not in out or param.kind is inspect.Parameter.VAR_KEYWORD:
            continue
        # `x: str = None` — a None default admits null even when the
        # annotation isn't Optional (pre-PEP-484 style is everywhere).
        if out[name] is None and param.default is None:
            continue
        ann = hints.get(name, param.annotation)
        try:
            out[name] = coerce(ann, out[name])
        except Exception as e:
            # A construction failure must not silently deliver a dict
            # where the function expects an object (docs/codegen.md).
            raise InvalidParams(f"param '{name}': {e}") from e
    return out


_CONTAINER_TYPES = (dict, list, tuple, set, frozenset)


def exceeds_depth(obj: Any, limit: int) -> bool:
    """True if `obj` nests containers deeper than `limit` JSON levels.

    Only containers add JSON nesting depth (serde's recursion limit
    counts `[`/`{` opens), so leaves are skipped instead of pushed —
    a million-element flat result is one cheap scan, not a million
    stack entries. Iterative: a recursive walk would hit Python's own
    recursion limit on exactly the payloads this is meant to catch.
    dict keys are JSON strings (never containers), so only values are
    descended.
    """
    stack = [(obj, 1)]
    while stack:
        node, depth = stack.pop()
        if depth > limit:
            return True
        if isinstance(node, dict):
            it = node.values()
        elif isinstance(node, _CONTAINER_TYPES):
            it = node
        else:
            continue
        stack.extend((v, depth + 1) for v in it if isinstance(v, _CONTAINER_TYPES))
    return False
