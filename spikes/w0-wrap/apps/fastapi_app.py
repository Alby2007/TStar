"""W0 command-mode sample: FastAPI on the port we are given.

Exercises the compat matrix: page load, text input, file upload +
download, an SSE stream, and an external link.
"""

import os

from fastapi import FastAPI, UploadFile
from fastapi.responses import FileResponse, HTMLResponse, StreamingResponse
from pydantic import BaseModel

app = FastAPI()
MARKER = "W0-WRAP-MARKER"


@app.get("/", response_class=HTMLResponse)
def index():
    return f"""<!doctype html><html><body>
<h1>{MARKER}</h1>
<form action="/upload" method="post" enctype="multipart/form-data">
  <input type="file" name="f"/><button>upload</button>
</form>
<a href="/download">download file</a>
<a href="https://tauri.app" target="_blank">external link</a>
<div id="log"></div>
<script>
const es = new EventSource("/stream");
es.onmessage = (e) => document.getElementById("log").textContent += e.data + " ";
// IPC-leak probe: report the Tauri surface this origin can actually see.
fetch("/ipc-probe", {{method: "POST", headers: {{"Content-Type": "application/json"}},
  body: JSON.stringify({{
    tauri: typeof window.__TAURI__,
    internals: typeof window.__TAURI_INTERNALS__,
    invoke: typeof window.__TAURI_INTERNALS__?.invoke,
    ua: navigator.userAgent,
  }})}}).catch(() => {{}});
</script>
</body></html>"""


class Probe(BaseModel):
    tauri: str
    internals: str
    invoke: str
    ua: str


@app.post("/ipc-probe")
def ipc_probe(p: Probe):
    # Lands in stderr → forwarded by the supervisor → visible in app logs.
    print(
        f"IPC-PROBE tauri={p.tauri} internals={p.internals} "
        f"invoke={p.invoke} ua={p.ua}",
        flush=True,
    )
    with open("/tmp/w0-ipc.json", "w") as fh:
        fh.write(p.model_dump_json())
    return {"ok": True}


@app.post("/upload")
async def upload(f: UploadFile):
    data = await f.read()
    return {"name": f.filename, "size": len(data)}


@app.get("/download")
def download():
    p = "/tmp/w0-download.txt"
    with open(p, "w") as fh:
        fh.write(MARKER)
    return FileResponse(p, filename="w0-download.txt")


@app.get("/stream")
def stream():
    async def gen():
        for i in range(5):
            yield f"data: tick{i}\n\n"

    return StreamingResponse(gen(), media_type="text/event-stream")


if __name__ == "__main__":
    import uvicorn

    uvicorn.run(app, host="127.0.0.1", port=int(os.environ["PYLAIR_PORT"]))
