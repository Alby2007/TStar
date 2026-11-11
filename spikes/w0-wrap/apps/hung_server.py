"""W0 fixture: binds the port, answers the FIRST request (so ready passes),
then accepts connections but never responds — alive but stuck. Proves why
pongs alone can't detect a wedged server (W1's HTTP probe rationale)."""

import os
import socket

with socket.socket() as s:
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.bind(("127.0.0.1", int(os.environ["PYLAIR_PORT"])))
    s.listen(8)
    first = True
    while True:
        conn, _ = s.accept()
        if first:
            conn.sendall(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
            conn.close()
            first = False
        # every later connection: held open, never answered
