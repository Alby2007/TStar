from pylair import expose
from pylair._expose import exposed_functions


def test_expose_bare_registers_and_calls():
    @expose
    def add(a: int, b: int) -> int:
        return a + b

    assert add(2, 3) == 5
    assert "add" in exposed_functions()


def test_expose_with_options():
    @expose(isolated=True)
    def heavy(path: str) -> dict:
        return {"path": path}

    assert heavy("/tmp/x") == {"path": "/tmp/x"}
    assert "heavy" in exposed_functions()
