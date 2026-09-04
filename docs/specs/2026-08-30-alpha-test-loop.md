# Orange Alpha Test Loop Specification

## Goal

Reduce a two-machine alpha test to opening one stable launcher on each machine,
running Orange, and reporting the visible result. The launcher updates Orange,
records the exact build and profile, and uploads diagnostics after Orange exits.

## Scope

- Add operation-level receive-branch timings so a missing completion identifies
  the exact blocking GStreamer call.
- Add non-secret build, run, device, profile, and media-session metadata to each
  JSONL diagnostic record.
- Give both peers the same random diagnostic session ID through signaling.
- Add an authenticated, size-bounded diagnostics endpoint to the relay.
- Store uploads either in a local directory for development or an Azure Blob
  container configured by a server-side SAS URL.
- Add a stable PowerShell alpha launcher with atomic, checksum-verified updates,
  local rollback, pending upload retry, and a persistent pseudonymous device ID.
- Add a publisher script that builds one portable ZIP, updates the private
  `alpha-latest` prerelease mirror, and uploads the public alpha channel to a
  dedicated Azure Blob container.
- Extend the Azure deployment script to provision diagnostics storage and pass
  its write-only container SAS to the relay.

## Constraints

- Existing unrelated changes in the main checkout must remain untouched.
- Diagnostics must never contain Discord tokens, authorization headers, room
  codes, SDP, ICE candidates, video, audio, window titles, or process names.
- HTTP uploads require a valid existing Orange session token and are limited to
  8 MiB compressed.
- Upload failures never block or fail streaming; pending archives retry on the
  next launcher run.
- On startup, the launcher archives any prior run directory left behind by a
  launcher, tray, media-process, or machine crash before retrying uploads.
- The launcher keeps the previous successful build and changes the active build
  only after SHA-256 verification and complete extraction.
- The alpha manifest schema is versioned and rejects unsupported schemas,
  non-HTTPS URLs, invalid commit IDs, and invalid SHA-256 values.
- Normal installers and non-alpha launches remain unchanged.
- The relay never stores diagnostics on ephemeral container storage when Blob
  storage is configured.
- New Rust behavior follows test-first development.

## Alpha Manifest

```json
{
  "schema": 1,
  "build": "0123456789abcdef0123456789abcdef01234567",
  "asset_url": "https://github.com/brnbtt/orange/releases/download/alpha-latest/orange-alpha-0123456.zip",
  "sha256": "64 uppercase hexadecimal characters",
  "profile": "hardware-bounded-jitter",
  "environment": {
    "ORANGE_AV1_DECODER": "hardware",
    "ORANGE_RTP_BUFFER_MODE": ""
  }
}
```

## Upload Contract

`POST /diagnostics` uses `Authorization: Bearer <Orange session token>`, body
`application/zip`, and these headers:

- `x-orange-build`: 40 lowercase hexadecimal characters
- `x-orange-run`: UUID
- `x-orange-device`: UUID
- `x-orange-profile`: lowercase letters, digits, and hyphens, maximum 64 bytes

The server generates the storage timestamp and derives the user directory from
a one-way SHA-256 digest of the authenticated Discord ID. It never uses client
metadata as an unrestricted filesystem or blob path.

## Storage Layout

```text
<build>/<UTC-date>/<user-digest>/<device>/<run>.zip
```

The archive contains only JSONL files produced beneath that launcher's unique
run directory plus a generated `run.json` containing build, run, device, and
profile. The launcher moves successful archives to `sent` and retains failed
archives in `pending`.

## Acceptance

- A second launcher run with the same manifest starts without downloading.
- A changed manifest downloads and activates a new build without overwriting the
  prior build.
- A checksum mismatch leaves the prior build active.
- Closing the tray creates one archive and attempts upload.
- A failed upload remains pending and succeeds on a later run.
- A run interrupted before archive creation is recovered on the next launch.
- Missing or invalid authentication receives `401`; malformed metadata receives
  `400`; oversized bodies receive `413`; disabled storage receives `503`.
- Host and viewer diagnostic logs contain the same diagnostic session ID.
- Receive branch logs identify every element creation, pipeline add, internal
  link, incoming pad link, and per-element state synchronization duration.
