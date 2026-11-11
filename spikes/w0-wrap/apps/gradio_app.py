"""W0 gradio sample: chat + file upload + file download + image display.
The launcher's gradio adapter pins host/port/share — the W0 spike calls
launch() directly to check baseline compat before adapters exist."""

import gradio as gr

MARKER = "W0-WRAP-MARKER"


def chat(msg, history):
    return f"{MARKER} echo: {msg}"


def save_file(f):
    return f"{MARKER} got {f.name if f else 'nothing'}"


with gr.Blocks(title="w0 gradio") as demo:
    gr.Markdown(f"# {MARKER}")
    gr.ChatInterface(chat, type="messages")
    gr.Interface(save_file, inputs=gr.File(), outputs=gr.Text())
    gr.Interface(lambda: MARKER, inputs=None, outputs=gr.File())
    gr.Interface(lambda p: p, inputs=gr.Image(), outputs=gr.Image())

if __name__ == "__main__":
    import os

    demo.launch(
        server_name="127.0.0.1",
        server_port=int(os.environ["PYLAIR_PORT"]),
        inbrowser=False,
        share=False,
    )
