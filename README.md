# Rust Voice Recorder (Resilient Long-Message Recorder)

This project is a **self-hostable web app** for recording long voice messages (like 1-hour notes) in a way that is much more reliable than in-app push-to-talk messaging.

Instead of recording and uploading in one fragile step, this app:

1. Records in the browser with `MediaRecorder`.
2. Splits audio into small chunks (for example every 5 seconds).
3. Uploads each chunk immediately to a Rust backend.
4. Keeps unsent chunks in browser storage and retries.
5. Creates a **private secret URL** per recording, with one-click copy and delete controls.

The goal is simple: **if network fails during recording, already-recorded audio is not lost**.

## Why this exists

Messaging apps can fail on very long voice notes because recording and upload are tightly coupled. For hour-long messages, a safer pattern is:

- record as chunks,
- persist what was already captured,
- resume upload after outages.

This project implements that reliability-first workflow in a memory-safe backend language (Rust).

## Privacy model

Each recording gets an automatically generated high-entropy secret token.

- Recorder URL: `/r/<very_long_secret>/`
- Playback URL: `/r/<very_long_secret>/file`

Only users with the secret URL can access that recording path.

> Important: this is secret-link protection, not full authentication/authorization. Add real auth before exposing to untrusted/public environments.

## Architecture

- **Frontend**: plain HTML + JavaScript (`getUserMedia` + `MediaRecorder` + IndexedDB)
- **Backend**: Rust + Axum + Tokio
- **Metadata DB**: SQLite
- **Chunk storage**: local disk
- **Output**: finalized file assembled server-side from ordered chunks

## Quick start

```bash
docker compose up -d --build
```

### Ports (internal vs host)

- **Internal app port (inside container):** `3000`
- **Example host binding (from `docker-compose.yml`):** `127.0.0.1:8080:3000`

That means:

- the app listens on `3000` in the container,
- and is reachable on `http://127.0.0.1:8080` from the host.

Click **Start** to create a recording session and auto-generate its private URL.

On the secret page you can also delete the recording (double-confirmation) and collapse/expand the debug console.

## Docker compose scope (no Caddy inside)

Per your deployment preference, `docker-compose.yml` only runs the recorder server.

If you want HTTPS/public exposure, run Caddy (or another reverse proxy) in a **separate Docker project** and proxy requests to this service.

Example for localhost-only publishing + Caddy in another container/project:

- Compose port mapping: `127.0.0.1:8080:3000` (host:container)
- Caddy upstream: `http://host.docker.internal:8080` (or your host IP)

Container images are published to GHCR and should include a rolling `:latest` tag from the default branch workflow.

## API overview

- `POST /api/sessions` -> create recording session and return secret URL details
- `POST /api/r/:secret/chunks` -> upload one chunk (`idx` + base64 audio bytes)
- `POST /api/r/:secret/finalize` -> assemble chunks into final audio file
- `GET /r/:secret/file` -> stream finalized audio for playback/download
- `DELETE /api/r/:secret` -> permanently delete a recording (chunks + finalized output + DB metadata)

Legacy/internal endpoints still exist for compatibility (`/api/sessions/:id/...`), but new UI flow uses secret-token routes.

## Reliability model

- Each emitted chunk is queued locally before upload attempt.
- If upload fails, chunk remains in queue and can be retried.
- Server deduplicates by `(session_id, idx)` so safe retries are possible.

## Limitations

- If browser/app crashes before the current chunk is emitted by `MediaRecorder`, that in-progress slice may be lost.
- Base64 chunk transport is easy to integrate but adds overhead.
- Secret URL alone is not equivalent to user authentication.

## Suggested production upgrades

- Authentication + per-user recording ownership
- Binary chunk uploads (multipart or octet-stream)
- S3/MinIO object storage backend
- Exponential backoff + background sync/service worker
- Optional transcoding/export pipeline

## Development

```bash
cargo fmt
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

---

This repository is a starter focused on **trustworthy long recording capture under unstable connectivity**.
