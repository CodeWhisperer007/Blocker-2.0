# Focuser — Agent Notes / Plan

## Context

- Project: Focuser — focus-blocking browser extension + local backend (Rust/Tauri + devserver).
- Browser extension: `extension/` (WXT + React + Vite). Backend base URL: `http://127.0.0.1:17549` (`API_BASE` in `extension/lib/api.ts`); calls `/api/rules`, `/api/heartbeat`, `/api/blocked`, `/api/allowance-tick`, `/api/extension-removed`.
- Web frontend: `crates/focuser-ui/frontend/` (Vite, dev server on port 1420). Devserver transport: `VITE_DEVSERVER_URL ?? "http://localhost:17550"`, calls `POST /command`; devserver serves `GET /health` and `POST /command`.
- Desktop app: `crates/focuser-ui/` (Tauri) — serves extension API at `127.0.0.1:17549`.
- Devserver: `crates/focuser-devserver/` — flags: `--port` (default 17550), `--memory`, `--db`, `--seed`.

## Current plan / state

- Goal: strip the desktop app source down to just the backend needed, with local-first design and optional cross-device sync.
- Backend target: standalone server on `127.0.0.1:17549` serving the extension's `/api/*` endpoints (the old standalone-service behavior absorbed into the desktop app in v0.4.2).
- Devserver currently used as frontend backend; planned switch to port 17549 via `cargo run -p focuser-devserver -- --port 17549 --memory` (run from workspace root).

## Optional changes

(none added — add only if asked)

## Asks Yourself
Everytime when you change or update the code make sure to idea based on the updated code will change the security level integratd wth the operating system (Windows, Linux or Android).