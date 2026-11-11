"""W0 command-mode sample: FastAPI on the port we are given.

Exercises the compat matrix: page load, text input, file upload +
download, an SSE stream, and an external link.
"""

import os

from fastapi import FastAPI, UploadFile
from fastapi.responses import FileResponse, HTMLResponse, StreamingResponse

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
</script>
</body></html>"""


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
