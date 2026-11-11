"""W0 streamlit sample: widgets + uploader + download. Multipage is an
extra apps/pages/ dir — added if baseline compat passes."""

import streamlit as st

MARKER = "W0-WRAP-MARKER"

st.set_page_config(page_title="w0 streamlit")
st.title(MARKER)

name = st.text_input("name")
if name:
    st.write(f"{MARKER} hello {name}")

f = st.file_uploader("upload")
if f:
    st.success(f"{MARKER} got {f.name} ({f.size} bytes)")

st.download_button("download", data=MARKER, file_name="w0-download.txt")
st.link_button("external link", "https://tauri.app")
