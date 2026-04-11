# GnomeAI-RS

Self-hosted Rust AI agent backend with a bundled web UI.

This folder is a publishable project snapshot. It includes only source code,
build files, the browser UI, and the optional WhatsApp bridge code. It does
not include chat history, uploaded files, generated documents, logs, auth
state, or personal configuration.

## Included

- Rust backend source under `src/`
- `Cargo.toml` and `Cargo.lock`
- `index.html` web UI
- optional WhatsApp bridge under `whatsapp/`
- clean `config.example.json`

## Requirements

Required:

- Rust stable with edition 2024 support
- an OpenAI-compatible API or `llama-server`

Optional:

- `tesseract` for OCR on images
- `pdftotext` from Poppler for PDF text extraction
- Node.js 18+ and `npm` for the WhatsApp bridge
- Firecrawl if you want web search and fetch tools

## Quick Start

1. Copy the example config:

```bash
cp config.example.json config.json
```

2. Edit `config.json`/ in web interface and set at least:

- `default_model`
- `llama_base_url`
- `llama_api_mode`
- `firecrawl_api_url` and `firecrawl_api_key` if you want web search

3. Run the server:

```bash
cargo run
```

4. Open the UI:

```text
http://127.0.0.1:8787
```

If you change `host` or `port` in `config.json`, use that address instead.

## Release Build

Build:

```bash
cargo build --release
```

Run:

```bash
./target/release/gnomef-rs
```

## Runtime Data

At runtime the server creates these folders next to the executable working
directory or inside `GNOMEF_RS_HOME` if that environment variable is set:

- `chats/`
- `uploads/`
- `generated/`
- `store/`

To keep runtime data outside the code folder:

```bash
mkdir -p ./data
cp config.example.json ./data/config.json
GNOMEF_RS_HOME=./data cargo run --release
```

The UI and optional bridge code can still stay in the project root.

## Optional WhatsApp Bridge

The Rust backend can start the Node bridge automatically, but the Node
dependencies must exist first:

```bash
cd whatsapp
npm install
cd ..
```

Then enable the feature in `config.json` /web interface :

- `whatsapp_enabled: true`
- `whatsapp_bridge_port`
- `whatsapp_assistant_name`

The backend exposes:

- `GET /api/whatsapp/status`
- `POST /api/whatsapp/start`
- `POST /api/whatsapp/stop`
- `GET /api/whatsapp/qr`

## Notes

- If `config.json` is missing, the server falls back to built-in defaults.
- `config.json` is ignored by Git in this export so local secrets do not get
  committed by accident.
- The project stores persistent memory in `store/memory.json`.
- The browser UI is a single static `index.html` file served by the backend.
