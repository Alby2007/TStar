"""Wire-codec unit tests — the JSON safety guards in pylair._json."""

import dataclasses

import pytest

from pylair import _json

_SAFE = (1 << 53) - 1  # JS Number.MAX_SAFE_INTEGER


def test_dumps_safe_int_boundary():
    assert _json.loads(_json.dumps({"n": _SAFE}))["n"] == _SAFE
    assert _json.loads(_json.dumps({"n": -_SAFE}))["n"] == -_SAFE


def test_dumps_rejects_int_above_safe_range():
    with pytest.raises(ValueError, match="safe-integer range"):
        _json.dumps({"n": _SAFE + 1})


def test_dumps_rejects_negative_int_below_safe_range():
    with pytest.raises(ValueError, match="safe-integer range"):
        _json.dumps({"n": -(_SAFE + 1)})


def test_dumps_rejects_nested_big_int():
    with pytest.raises(ValueError, match="safe-integer range"):
        _json.dumps({"a": [{"b": 1 << 60}]})


@dataclasses.dataclass
class _Point:
    x: int
    y: int


def test_dumps_rejects_big_int_inside_dataclass():
    # The guard runs on the emitted text, so conversions in _default
    # can't smuggle an unsafe int past it.
    with pytest.raises(ValueError, match="safe-integer range"):
        _json.dumps(_Point(1, 1 << 60))


def test_dumps_rejection_message_names_workaround():
    with pytest.raises(ValueError, match="as a string"):
        _json.dumps({"n": 1 << 60})


def test_dumps_still_rejects_nan():
    with pytest.raises(ValueError):
        _json.dumps({"x": float("nan")})


def test_dumps_float_unaffected():
    # Big floats keep double semantics — only int precision is guarded.
    assert _json.loads(_json.dumps({"x": 1e300}))["x"] == 1e300
