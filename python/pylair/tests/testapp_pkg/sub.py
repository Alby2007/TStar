"""Fixture submodule: an isolated function the app module does not re-export."""

from pylair import expose


@expose(isolated=True)
def sub_ping(text: str) -> dict:
    return {"echoed": text, "via": "testapp_pkg.sub"}
