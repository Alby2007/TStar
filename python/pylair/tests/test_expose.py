import importlib

import pytest

import pylair._expose as _ex
from pylair import expose
from pylair._expose import exposed_functions


def _with_clean_registry(fn):
    """Run fn() with a private registry, restoring the real one after."""
    saved = dict(_ex._registry)
    _ex._registry.clear()
    try:
        return fn()
    finally:
        _ex._registry.clear()
        _ex._registry.update(saved)


def test_expose_bare_registers_and_calls():
    @expose
    def add_numbers(a: int, b: int) -> int:
        return a + b

    assert add_numbers(2, 3) == 5
    assert "add_numbers" in exposed_functions()


def test_expose_with_options():
    @expose(isolated=True)
    def heavy(path: str) -> dict:
        return {"path": path}

    assert heavy("/tmp/x") == {"path": "/tmp/x"}
    assert "heavy" in exposed_functions()


def test_duplicate_names_across_modules_raise():
    def run():
        importlib.import_module("dupmod_a")
        with pytest.raises(ValueError) as e:
            importlib.import_module("dupmod_b")
        msg = str(e.value)
        assert "ping" in msg
        assert "dupmod_a" in msg, f"error must name the first definition: {msg}"
        assert "dupmod_b" in msg, f"error must name the new definition: {msg}"
        # The first registration still holds — nothing was overwritten.
        assert _ex._registry["ping"].fn.__module__ == "dupmod_a"

    _with_clean_registry(run)


def test_reloading_a_module_replaces_silently():
    def run():
        import dupmod_a

        importlib.reload(dupmod_a)
        before = _ex._registry["ping"].fn
        importlib.reload(dupmod_a)  # hot reload: same module+qualname
        after = _ex._registry["ping"].fn
        assert after.__module__ == "dupmod_a"
        assert after is not before, "registry must hold the LATEST definition"

    _with_clean_registry(run)
