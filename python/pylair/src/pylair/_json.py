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


# ---- input coercion: dict -> annotated types ------------------------------


def coerce(annotation: Any, value: Any) -> Any:
    """Rebuild `value` (wire JSON) into `annotation`'s type where possible."""
    if annotation is inspect.Parameter.empty or value is None:
        return value

    origin = typing.get_origin(annotation)
    args = typing.get_args(annotation)

    # Optional[T] / Union[...] — try each arm, first success wins.
    if origin in (typing.Union, types.UnionType):
        for arm in args:
            if arm is type(None):
                continue
            try:
                return coerce(arm, value)
            except Exception:  # noqa: BLE001,S112 - try the next union arm
                continue
        return value

    if inspect.isclass(annotation) and dataclasses.is_dataclass(annotation):
        if isinstance(value, dict):
            fields = {f.name: f for f in dataclasses.fields(annotation)}
            try:
                # Under `from __future__ import annotations` f.type is a
                # string — resolved hints are required for nested types.
                hints = typing.get_type_hints(annotation)
            except Exception:  # noqa: BLE001 - fall back to raw field types
                hints = {}
            kwargs = {
                k: coerce(hints.get(k, fields[k].type), v)
                for k, v in value.items()
                if k in fields
            }
            return annotation(**kwargs)
        return value

    if origin in (list, set, frozenset) or annotation is list:
        (item_t,) = args or (inspect.Parameter.empty,)
        if isinstance(value, list):
            items = [coerce(item_t, v) for v in value]
            return set(items) if origin in (set, frozenset) else items
        return value

    if origin is tuple and isinstance(value, list):
        if len(args) == 2 and args[1] is Ellipsis:
            return tuple(coerce(args[0], v) for v in value)
        return tuple(coerce(t, v) for t, v in zip(args, value, strict=False))

    if origin is dict or annotation is dict:
        val_t = args[1] if len(args) == 2 else inspect.Parameter.empty
        if isinstance(value, dict):
            return {k: coerce(val_t, v) for k, v in value.items()}
        return value

    if annotation in (_dt.datetime, _dt.date, _dt.time) and isinstance(value, str):
        try:
            return annotation.fromisoformat(value)
        except ValueError:
            return value

    if annotation is bytes and isinstance(value, str):
        # TS may send a bare base64 string for a bytes-annotated param.
        try:
            return base64.b64decode(value)
        except Exception:  # noqa: BLE001 - not base64; pass through
            return value

    if inspect.isclass(annotation) and issubclass(annotation, enum.Enum):
        try:
            return annotation(value)
        except ValueError:
            return value

    return value


def coerce_kwargs(fn: Any, input: dict) -> dict:
    """Coerce an incoming `input` dict by `fn`'s parameter annotations."""
    try:
        sig = inspect.signature(fn)
    except (TypeError, ValueError):
        return input
    try:
        hints = typing.get_type_hints(fn)
    except Exception:  # noqa: BLE001 - unresolved refs fall back to raw anns
        hints = {}
    out = dict(input)
    for name, param in sig.parameters.items():
        if name not in out:
            continue
        ann = hints.get(name, param.annotation)
        try:
            out[name] = coerce(ann, out[name])
        except Exception as e:
            # A construction failure must not silently deliver a dict
            # where the function expects an object (docs/codegen.md).
            raise InvalidParams(f"param '{name}': {e}") from e
    return out
