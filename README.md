# Logs Uploader

A small, privacy-respecting Warcraft Logs uploader built with [Tauri](https://tauri.app)
(Rust backend, WebView2 frontend). It does what the official uploader does —
upload a finished combat log, or live-log a raid — and nothing else:

- no Overwolf, no ads SDK, no game-event capture, no crash/telemetry reporting;
- the only network traffic is to the Warcraft Logs site you log in to;
- credentials are held in memory for the session only (the email can optionally
  be remembered locally).

## How it works

Warcraft Logs does not ship its combat-log parser with the client: the site
serves it as JavaScript from `/desktop-client/parser`. The official app runs it
in a sandboxed browser iframe; this app instead runs the very same JavaScript in
a small **Node.js sidecar** (`src-tauri/resources/parser-harness.js`) driven over
stdin/stdout. That avoids WebView2/Cloudflare/session friction entirely. The Rust
side owns the whole flow — login, fetching the parser code, chunked file reading,
zipping, uploading segments, retries — and the sidecar only ever parses lines and
hands back fights. The complete reconstructed protocol is in
[docs/PROTOCOL.md](docs/PROTOCOL.md).

Because the parser JavaScript comes from the server, patch-day format changes are
handled upstream; this app never needs to understand the combat log format itself.

Requests to the parser route must use an Electron-like `User-Agent`
(`wcl::USER_AGENT`) or the server returns 404.

Requirement: **Node.js 18+** must be on `PATH` (or point `LU_NODE` at a `node`
executable). Bundling a Node runtime with the installer is a TODO.

## Building

Prerequisites: Rust (stable), Node.js, and on Windows the MSVC build tools
and the WebView2 runtime (bundled with Windows 10/11).

```bash
npm install
npm run dev      # run with hot reload
npm run build    # produce src-tauri/target/release/logs-uploader.exe + installer
```

Backend unit tests:

```bash
cd src-tauri && cargo test
```

## Layout

```
frontend/            plain HTML/CSS/JS UI (no bundler)
src-tauri/src/
  wcl/               HTTP client for the desktop-client API + parser-code fetch
  parser.rs          Node-sidecar parser driver (spawn + stdin/stdout protocol)
  logfile.rs         chunked log reading, header priming, zip payloads
  operation.rs       upload-a-log and live-log state machines
  settings.rs        remembered email/password (OS keychain) + report options
  commands.rs        Tauri commands exposed to the UI
  resources/parser-harness.js   Node host that runs the site's parser JS
docs/PROTOCOL.md     the wire protocol, reconstructed from the official client
```

## Notes

- `wcl::CLIENT_VERSION` is the official uploader release the protocol was
  reconstructed from; the server checks a minimum client version, so bump it if
  the site starts refusing logins.
- Using a third-party client is against the Warcraft Logs terms of service.
  Your account, your call.
