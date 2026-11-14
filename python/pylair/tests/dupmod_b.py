"""Fixture: exposes ping() — collides with dupmod_a.ping on purpose."""

from pylair import expose


@expose
def ping() -> str:
    return "b"
