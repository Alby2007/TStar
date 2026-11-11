"""M2: schema export, wire JSON round-trip, param coercion, manifest."""

import datetime
import subprocess
import sys
import typing
from dataclasses import dataclass

import pytest

sys.path.insert(0, str(__file__.rsplit("/", 1)[0]))

import testapp
from tauri_plus import _json, _schema
from tauri_plus._expose import exposes_manifest


@dataclass
class Nested:
    p: testapp.Point
    n: int = 0


# ---- schema mapping --------------------------------------------------------


def test_schema_primitives():
    s = _schema.schema_of
    assert s(int) == {"type": "int"}
    assert s(float) == {"type": "float"}
    assert s(str) == {"type": "str"}
    assert s(bool) == {"type": "bool"}
    assert s(None) == {"type": "none"}
    assert s(type(None)) == {"type": "none"}
    assert s(bytes) == {"type": "bytes"}
    assert s(datetime.datetime) == {"type": "datetime"}
    assert s(typing.Any) == {"type": "any"}


def test_schema_containers():
    s = _schema.schema_of
    assert s(list[int]) == {"type": "list", "items": {"type": "int"}}
    assert s(tuple[int, str]) == {
        "type": "tuple",
        "items": [{"type": "int"}, {"type": "str"}],
    }
    assert s(tuple[int, ...]) == {"type": "list", "items": {"type": "int"}}
    assert s(dict[str, int])["values"] == {"type": "int"}
    assert s(list[testapp.Point])["items"]["name"] == "Point"


def test_schema_union_literal_enum():
    s = _schema.schema_of
    u = s(str | None)
    assert u == {"type": "union", "opts": [{"type": "str"}, {"type": "none"}]}
    assert s(typing.Literal["a", "b"]) == {
        "type": "literal",
        "values": ["a", "b"],
    }
    assert s(testapp.Color) == {"type": "enum", "values": ["red", "blue"]}


def test_schema_dataclass_recursive():
    sc = _schema.schema_of(testapp.Point)
    assert sc["type"] == "dataclass" and sc["name"] == "Point"
    fields = {f["name"]: f for f in sc["fields"]}
    assert fields["x"]["schema"] == {"type": "float"}
    assert fields["x"]["required"] is True
    assert fields["label"]["required"] is False
    nested = _schema.schema_of(Nested)
    pfield = next(f for f in nested["fields"] if f["name"] == "p")
    assert pfield["schema"]["name"] == "Point"


def test_callable_schema_generator_yield():
    sig = next(e["signature"] for e in exposes_manifest() if e["name"] == "numbers")
    # For generators, `returns` is the yield type — int, not asyncgen.
    assert sig["returns"] == {"type": "int"}


def test_unannotated_is_any():
    sig = next(e["signature"] for e in exposes_manifest() if e["name"] == "counter")
    assert sig["returns"] == {"type": "any"}


# ---- wire JSON -------------------------------------------------------------


def test_wire_roundtrip_types():
    payload = {
        "data": b"\x00\x01\x02",
        "p": testapp.Point(1.5, 2.5, "a"),
        "when": datetime.datetime(2025, 1, 2, 3, 4, 5),
        "c": testapp.Color.BLUE,
        "tags": {"b", "a"},
    }
    back = _json.loads(_json.dumps(payload))
    assert back["data"] == b"\x00\x01\x02"
    assert back["p"] == {"x": 1.5, "y": 2.5, "label": "a"}
    assert back["when"] == "2025-01-02T03:04:05"
    assert back["c"] == "blue"
    assert back["tags"] == ["a", "b"]


def test_wire_rejects_unserializable():
    with pytest.raises(TypeError):
        _json.dumps({"f": object()})


# ---- coercion --------------------------------------------------------------


def test_coerce_dataclass_and_enum():
    kw = _json.coerce_kwargs(
        testapp.move, {"p": {"x": 1.0, "y": 2.0, "label": "L"}, "dx": 3.0}
    )
    assert kw["p"] == testapp.Point(1.0, 2.0, "L")
    c = _json.coerce_kwargs(testapp.pick, {"c": "red"})
    assert c["c"] is testapp.Color.RED


def test_coerce_optional_and_passthrough():
    kw = _json.coerce_kwargs(testapp.pick, {"c": "blue", "fallback": None})
    assert kw["c"] is testapp.Color.BLUE and kw["fallback"] is None
    # unknown annotation shapes pass the wire value through untouched
    kw = _json.coerce_kwargs(testapp.add, {"a": 1, "b": 2})
    assert kw == {"a": 1, "b": 2}


# ---- manifest entrypoint ---------------------------------------------------


def test_codegen_manifest_subprocess():
    out = subprocess.run(
        [
            sys.executable,
            "-m",
            "tauri_plus.codegen",
            "testapp",
        ],
        capture_output=True,
        text=True,
        check=False,
        cwd=str(__file__.rsplit("/", 1)[0]),
    )
    assert out.returncode == 0, out.stderr
    manifest = _json.loads(out.stdout)
    assert manifest["version"] == 1
    fns = {f["name"]: f for f in manifest["functions"]}
    assert fns["move"]["params"][0]["schema"]["name"] == "Point"
    assert fns["move"]["returns"]["name"] == "Point"
    assert fns["numbers"]["kind"] == "generator"
    assert fns["numbers"]["returns"] == {"type": "int"}
    assert fns["move"]["doc"].startswith("Translate a point")


def test_codegen_manifest_bad_module():
    out = subprocess.run(
        [sys.executable, "-m", "tauri_plus.codegen", "no_such_mod_xyz"],
        capture_output=True,
        text=True,
        check=False,
    )
    assert out.returncode == 1
    assert "error" in _json.loads(out.stdout)


# ---- end-to-end through a real runtime -------------------------------------


def test_typed_values_over_the_wire(runtime):
    """Dataclass/bytes/enum params and results survive a real runtime."""
    hello = runtime.recv()
    assert hello["method"] == "hello"
    sig = {e["name"]: e["signature"] for e in hello["params"]["exposes"]}
    assert sig["move"]["params"][0]["schema"]["name"] == "Point"
    assert sig["move"]["returns"]["name"] == "Point"

    runtime.send(
        {
            "jsonrpc": "2.0",
            "id": 1,
            "method": "call",
            "params": {
                "fn": "move",
                "task": 1,
                "input": {"p": {"x": 1.0, "y": 2.0}, "dx": 5.0},
            },
        }
    )
    resp = runtime.recv()
    assert resp["result"] == {"x": 6.0, "y": 2.0, "label": ""}

    runtime.send(
        {
            "jsonrpc": "2.0",
            "id": 2,
            "method": "call",
            "params": {
                "fn": "roundtrip",
                "task": 2,
                "input": {"data": {"$b64": "AAEC"}},
            },
        }
    )
    resp = runtime.recv()
    assert resp["result"] == {"$b64": "AgEA"}
