# Settings Troubleshooting Implementation Plan

**Goal:** Give users measured readiness checks and a copyable explanation of
recent connection failures from Settings.

**Spec:** `docs/specs/2026-09-07-troubleshoot.md`

**Architecture:** The desktop launches a bounded diagnostic media child using
its existing runtime environment and process ownership. The child checks local
media capabilities and network reachability without joining or opening a room.
The desktop combines that result with bounded local historical evidence.

**Tech stack:** Existing Rust, GStreamer, Tokio, serde_json and GPUI dependencies.

## Constraints

- No new dependencies, TURN service, or deployment changes.
- No global ready verdict based on STUN or factory availability.
- Fixed, sanitized report text; no raw errors, tokens, addresses or identities.
- Single cancellable job, bounded child output and lifetime, owned cleanup.

## 1. Diagnostic media command

Files: `crates/orange/src/main.rs`, new `crates/orange/src/troubleshoot.rs`.

- [x] Add `orange troubleshoot --server <configured websocket URL>`.
- [x] Add deterministic tests for a valid STUN binding response, mismatched
  transaction ID, truncated attributes, and UDP deadline expiration. Use a
  local UDP responder, never public networking in ordinary tests.
- [x] Probe runtime/factory availability and the existing automatic encoder
  selection. Describe these as capabilities, not actual capture/playback.
- [x] Check the configured signalling WebSocket without creating a room.
- [x] Check UDP STUN reachability with bounded DNS, retries and reads; retain
  no mapped address in the result.
- [x] Emit a versioned JSON report with check IDs, statuses and fixed details.
- [x] Run targeted tests and exercise the real command on this machine.

## 2. Desktop job and Settings report

Files: `crates/orange-client/src/main.rs`, `supervisor.rs`,
`view/settings.rs`, new `troubleshoot.rs` and relevant test module.

- [x] Test child success, malformed output, failure, timeout, cancellation and
  duplicate starts before implementing the job.
- [x] Reuse executable/runtime lookup and bounded child collection. Exclude
  arbitrary stderr from user-copyable output.
- [x] Add a single owned job, polled by the existing UI tick. Cancellation
  returns promptly; completion or shutdown reaps the child and worker.
- [x] Add Troubleshoot, running feedback, Cancel, Run again and Copy report
  using the existing Settings cards and keyboard-focus controls.
- [x] Show current checks separately from dated historical evidence and state
  that capture, physical playback and connection to a friend remain untested.

## 3. Historical evidence

Files: new `crates/orange-client/src/troubleshoot/history.rs`.

- [x] Test the reported Checking-to-Failed sequence and connected-without-media
  separately. Also cover received media, incomplete JSONL, malformed fields,
  mixed host viewer roles and old evidence followed by a newer session.
- [x] Read bounded recent log tails off the UI thread, including open files
  whose directory metadata says zero bytes. Trust record timestamps for
  chronology and include allowlisted build metadata.
- [x] Return measured failure stages with next steps; never infer NAT type,
  firewall attribution, healthy physical output, or zero packet loss.

## 4. Verification and documentation

- [x] Update `ARCHITECTURE.md` source map and diagnostic command contract, and
  document Settings usage in `README.md`.
- [x] Run `cargo test --locked --workspace --all-features` with
  `RUST_TEST_THREADS=1` (baseline parallel suite hit documented child deadlines).
- [x] Run `cargo clippy --locked --workspace --all-targets --all-features -- -D warnings`.
- [x] Run `cargo fmt --all -- --check` and review the final diff.

Baseline: serial workspace tests passed: 206 media, 112 client, 57 signal,
9 updater; five intentionally ignored tests.

## Execution evidence

- All-feature serial workspace gate: 412 passed, five ignored; clippy with
  warnings denied, formatting check and workspace development build passed.
- After review added two client contract/text-boundary regressions: client
  all-feature suite passed with 125 tests and two ignored; client clippy,
  formatting and development build passed again (414 total passing tests).
- Actual command against production signalling: seven checks passed on this
  Windows development machine, including the WebSocket handshake and IPv4 STUN.
- The new history reader classified the two supplied viewer logs as ICE
  connectivity failures and kept the earlier host session separate.
- Disposable native GPUI fixture at 150% scaling (720 x 990): inspected idle,
  running, result and scrolled layouts; cancellation returned to idle; Tab then
  Enter copied the actual report with signalling failure distinct from STUN
  success. The fixture uses an intentionally unused signalling port.
- Optional titlebar-drag harness stopped before its drag assertions because
  Windows did not grant the fixture foreground ownership.
- No two-peer streaming or physical capture/playout acceptance was performed
  for this diagnostic-only feature.
