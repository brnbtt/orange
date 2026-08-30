# Orange Alpha Test Loop Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a one-launch alpha update and diagnostics pipeline for repeatable two-machine media debugging.

**Architecture:** A stable PowerShell launcher downloads commit-addressed portable builds from a public-read Azure Blob channel, injects pseudonymous run metadata, and uploads completed ZIP archives with retry. Orange emits precise operation spans and a relay-generated media-session ID; the authenticated relay validates each archive and stores it through a separate private local or Azure Blob backend. GitHub prerelease assets remain an authenticated mirror because the source repository is private.

**Tech Stack:** Rust 2021, GStreamer, Axum 0.7, Reqwest 0.12, PowerShell 5.1+, GitHub Releases, Azure Container Apps, Azure Blob Storage.

**Spec:** `docs/superpowers/specs/2026-08-30-alpha-test-loop.md`

## Global Constraints

- Preserve all unrelated main-worktree changes.
- Never log or upload secrets, room codes, SDP, ICE, media, window titles, or process names.
- Require Orange session authentication and cap compressed uploads at 8 MiB.
- Upload and update failures must not prevent streaming or destroy the previous usable build.
- Keep normal installer and non-alpha behavior unchanged.
- Use test-first development for new Rust and testable PowerShell behavior.

---

### Task 1: Diagnostic Context And Operation Timings

**Files:**
- Modify: `crates/orange/src/media_diagnostics.rs`
- Modify: `crates/orange/src/webrtc.rs`
- Modify: `crates/orange/src/peer.rs`

**Interfaces:**
- Produces: JSONL records with `elapsed_ms`, `build`, `run`, `device`, and `profile` top-level fields sourced only from `ORANGE_BUILD_ID`, `ORANGE_RUN_ID`, `ORANGE_DEVICE_ID`, and `ORANGE_TEST_PROFILE`.
- Produces: `measure_diagnostic_operation(role, operation, closure) -> closure result`, emitting `operation-started` and `operation-finished` with duration and success.
- Produces: `diagnostic-session` events emitted when signaling supplies the shared media-session ID.

- [ ] Write tests proving metadata is omitted when absent, included when valid, operation completion records duration/success, and no secret environment variables are serialized.
- [ ] Run the focused tests and confirm they fail because the context and operation API do not exist.
- [ ] Implement context serialization with one process-relative monotonic clock and a minimal operation measurement helper.
- [ ] Add operation markers around receive element creation, pipeline insertion, internal linking, incoming pad linking, each state synchronization, and final probe release.
- [ ] Run focused tests and `cargo test -p orange`.
- [ ] Commit the task.

### Task 2: Shared Diagnostic Session Signaling

**Files:**
- Modify: `crates/orange-signal/src/lib.rs`
- Modify: `crates/orange/src/peer.rs`

**Interfaces:**
- Produces: optional `diagnostic_session: String` on `Signal::Hosting` and `Signal::StreamInfo`, defaulting during deserialization for older peers.
- Consumes: Task 1 diagnostic event API.

- [ ] Write relay tests proving one random diagnostic session is sent to both host and viewer and room codes are not reused as that identifier.
- [ ] Run the focused tests and confirm the new fields or assertions fail.
- [ ] Generate one diagnostic session per hosted room and forward it in both signaling responses.
- [ ] Emit `diagnostic-session` on host and viewer receipt without emitting the room code.
- [ ] Run `cargo test -p orange-signal` and `cargo test -p orange`.
- [ ] Commit the task.

### Task 3: Authenticated Diagnostics Upload

**Files:**
- Modify: `Cargo.toml`
- Modify: `crates/orange-signal/Cargo.toml`
- Modify: `crates/orange-signal/src/auth.rs`
- Modify: `crates/orange-signal/src/server.rs`

**Interfaces:**
- Produces: `POST /diagnostics` implementing the upload contract in the spec.
- Produces: `DiagnosticsStore` configured from `ORANGE_DIAGNOSTICS_DIRECTORY` for development or `ORANGE_DIAGNOSTICS_CONTAINER_URL` for Blob Storage.
- Produces: deterministic metadata validation and storage-key construction functions covered without network access.

- [ ] Write tests for missing/invalid auth, invalid metadata, disabled storage, accepted local upload, path confinement, and exact storage-key shape.
- [ ] Run focused tests and confirm route and validation failures.
- [ ] Implement strict header validation, bearer authentication through `Auth::identify`, an 8 MiB Axum body limit, SHA-256 user pseudonyms, and local atomic writes.
- [ ] Implement Azure Blob `PUT` using the server-side SAS URL, `x-ms-blob-type: BlockBlob`, bounded timeout, and `error_for_status`.
- [ ] Run `cargo test -p orange-signal` and `cargo test --workspace`.
- [ ] Commit the task.

### Task 4: Stable Alpha Launcher

**Files:**
- Create: `alpha/orange-alpha.cmd`
- Create: `alpha/orange-alpha.ps1`
- Create: `alpha/test-orange-alpha.ps1`

**Interfaces:**
- Consumes: the manifest and upload contracts in the spec.
- Produces: `%LOCALAPPDATA%\Orange Alpha\versions`, `diagnostics`, `pending`, `sent`, `active.json`, and `device-id.txt`.
- Produces: environment inherited by the tray and media children: `ORANGE_BUILD_ID`, `ORANGE_RUN_ID`, `ORANGE_DEVICE_ID`, `ORANGE_TEST_PROFILE`, `ORANGE_MEDIA_DIAGNOSTICS`, and manifest-selected media settings.

- [ ] Write a self-contained PowerShell test harness with local fixture manifests/assets covering first install, cache hit, update, checksum rejection, pending retry, and unsupported manifest input.
- [ ] Run the harness and confirm it fails because launcher functions are absent.
- [ ] Implement strict manifest validation, HTTPS-only production downloads, SHA-256 verification, temporary extraction plus atomic activation, and one-version rollback retention.
- [ ] Implement run metadata, archive creation after tray exit, recovery of unarchived prior run directories, bearer upload from the existing session file, pending retry, and non-blocking failure reporting.
- [ ] Run the PowerShell harness twice and confirm no network dependency.
- [ ] Commit the task.

### Task 5: Repeatable Alpha Publishing

**Files:**
- Create: `alpha/publish-alpha.ps1`
- Modify: `README.md`

**Interfaces:**
- Produces: `dist/alpha/orange-alpha-<short-sha>.zip` and `dist/alpha/orange-alpha.json`.
- Publishes only with explicit `-Publish`; otherwise builds and validates locally.
- Publishes commit-addressed ZIP assets plus stable manifest/launcher assets to the Azure alpha channel and mirrors them to prerelease tag `alpha-latest`.

- [ ] Add dry-run script checks for manifest schema, full commit ID, uppercase SHA-256, archive contents, and stable launcher URLs.
- [ ] Run dry-run and confirm missing publisher behavior fails.
- [ ] Implement test, locked release build, staging, hashing, manifest generation, Azure Blob upload, and opt-in `gh release create/upload --clobber` mirror behavior.
- [ ] Document the one-time launcher installation, test flow, storage privacy, local paths, and publish command.
- [ ] Run publisher without `-Publish` and run the PowerShell launcher tests.
- [ ] Commit the task.

### Task 6: Azure Diagnostics Provisioning

**Files:**
- Modify: `deploy/azure.ps1`
- Create: `deploy/test-azure-script.ps1`

**Interfaces:**
- Produces: a private diagnostics Blob container with lifecycle retention and a server-side write/create SAS stored as Container Apps secret `diag-container-url`, plus a separate public-read `releases` container for alpha binaries.
- Configures: `ORANGE_DIAGNOSTICS_CONTAINER_URL` from a secret reference without printing the SAS.

- [ ] Add static script tests proving secure-transfer-only storage, disabled public blob access, private container creation, retention configuration, secret reference usage, and absence of SAS output.
- [ ] Run the checks and confirm they fail against the current deployment script.
- [ ] Extend deployment to idempotently provision storage, container, retention, SAS secret `diag-container-url`, and relay environment configuration.
- [ ] Run static checks and PowerShell parser validation without changing Azure resources.
- [ ] Commit the task.

### Task 7: End-To-End Verification

**Files:**
- Modify only if verification exposes a defect in files from Tasks 1-6.

**Interfaces:**
- Consumes all prior task interfaces.
- Produces a locally validated alpha ZIP, manifest, and launcher bundle; deployment and GitHub publication remain explicit external actions.

- [ ] Run `. .\dev.ps1; cargo fmt --check; cargo test --workspace; cargo build --locked --release -p orange -p orange-tray -p orange-relay`.
- [ ] Run all PowerShell harnesses and `git diff --check`.
- [ ] Run the launcher against a local fixture to prove update, launch, archive, failed-upload retention, and retry without contacting production.
- [ ] Review the complete branch for security, privacy, failure isolation, and regressions.
- [ ] Commit any verification fixes separately and re-run the affected checks.
