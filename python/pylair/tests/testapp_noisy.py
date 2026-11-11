"""Noisy fixture: prints at import and inside calls. The runtime's stdout
hygiene (fd 1 -> stderr, real stdout reserved for frames) must keep the
protocol channel clean — a stray print must neither corrupt the handshake
nor be mistaken for a readiness signal.
"""

from pylair import expose

print("READY")  # looks like a readiness banner; must not trip the handshake
print("testapp_noisy: import-time print")


@expose
def add(a: int, b: int) -> int:
    print(f"add called: {a} + {b}")
    return a + b
