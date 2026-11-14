"""Fixture: exposes ping() — collides with dupmod_b.ping on purpose."""

from pylair import expose


@expose
def ping() -> str:
    return "a"
