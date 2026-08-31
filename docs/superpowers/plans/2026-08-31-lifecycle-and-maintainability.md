# Lifecycle And Maintainability Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give every long-lived Orange worker one deterministic owner, then arrange the code and documentation so a new contributor can find each behavior and its tests without tracing thousand-line mixed-responsibility files.

**Architecture:** First replace detached native, standard-thread, and Tokio work with small owners local to the feature that starts each worker. Once teardown behavior is covered, move existing cohesive code into feature modules without redesigning it. Finish with an architecture map, corrected product documentation, and one documented verification path.

**Tech Stack:** Rust 1.98, Rust 2021, Tokio, GStreamer 1.26 Rust bindings, GPUI 0.2, Win32, Axum 0.7, PowerShell 5.1, Inno Setup 6.

**Spec:** `docs/superpowers/specs/2026-08-30-production-readiness.md`

## Global Constraints

- Preserve the frozen H.265, Opus, RTP payload, jitterbuffer, installer, update, diagnostics, and signaling behavior in the production-readiness spec.
- Do not add a worker framework, service locator, dependency-injection layer, component system, or new runtime dependency.
- A cloneable handle may observe or signal a resource; only one non-cloneable owner may stop and join it.
- Never join a worker from its own callback, a GStreamer streaming callback, a Win32 window procedure, or while holding an overlay/registry mutex.
- Put pipelines into `Null` before closing a playback HWND used by `d3d11videosink`.
- Use test-first red/green cycles for lifecycle behavior. Purely mechanical file moves must have zero behavior diff and preserve all existing tests.
- Run `. .\dev.ps1` before Orange media tests, Clippy, and builds.
- Do not modify or clean the user-owned `main` worktree, `.gitignore`, `.agents/`, or `skills-lock.json`.

---

### Task 1: Own Signaling Client Tasks

**Files:**
- Modify: `crates/orange-signal/src/lib.rs:420-520`
- Test: `crates/orange-signal/src/lib.rs`

**Interfaces:**
- Produces: `SignalClient` fields for both spawned task handles.
- Produces: `SignalClient::close(self)` that sends the close request, waits boundedly for graceful completion, aborts unfinished tasks, and awaits both handles.
- Preserves: public `outgoing`, public `incoming`, and all serialized `Signal` forms.

- [ ] **Step 1: Add a failing real-WebSocket test**

Add a test that starts the local Axum relay, connects through `connect`, closes the client, and observes through a test-only drop signal that both task futures released their captured resources before `close` returns.

- [ ] **Step 2: Verify the test fails because the reader task is not owned**

Run: `cargo test -p orange-signal signal_client_close_reaps_both_socket_tasks -- --exact`

- [ ] **Step 3: Retain both `JoinHandle<()>` values in `SignalClient`**

Move the two `tokio::spawn` return values into private fields. Keep the existing close-frame path; after its 500 ms grace period, abort any unfinished task and await both handles so cancellation is complete before return.

- [ ] **Step 4: Run focused and crate tests**

Run: `cargo test -p orange-signal --locked`

- [ ] **Step 5: Commit**

```text
git add crates/orange-signal/src/lib.rs
git commit -m "Own signaling client tasks"
```

### Task 1A: Preserve Live Rooms On Code Collisions

**Files:**
- Modify: `crates/orange-signal/src/relay.rs` after the Task 7 move, or the current `crates/orange-signal/src/lib.rs:271-287` before that move
- Test: the same module

**Interfaces:**
- Produces: private room insertion helper that accepts a code generator and retries while a generated code is occupied.
- Preserves: six-character room code format and every signaling message.

- [ ] **Step 1: Add a failing deterministic collision test**

Provide one occupied code followed by one free code. Assert hosting preserves the existing room and inserts the new room under the free code.

- [ ] **Step 2: Verify the test fails because `HashMap::insert` replaces the occupied room**

Run: `cargo test -p orange-signal room_code_collision_preserves_the_live_room -- --exact`

- [ ] **Step 3: Retry generation under the existing room mutex**

Generate until `rooms.contains_key(&code)` is false, then insert. Keep generation and insertion in the same critical section so two hosts cannot claim one code.

- [ ] **Step 4: Remove the room credential from production logs**

Change room-close logging to a fixed event/message with no room code, peer id, identity, SDP, ICE, OAuth value, or session token.

- [ ] **Step 5: Run relay tests and commit**

Run: `cargo test -p orange-signal --locked`

```text
git add crates/orange-signal/src
git commit -m "Preserve rooms across code collisions"
```

### Task 2: Give Playback Windows One Thread Owner

**Files:**
- Modify: `crates/orange/src/window.rs`
- Modify: `crates/orange/src/main.rs:213-322,331-414`
- Modify: `crates/orange/src/webrtc.rs:143-220,469-611`
- Modify: `crates/orange/src/peer.rs:96-160,1326-1528`
- Modify: `crates/orange/src/overlay.rs:1080-1133`
- Modify: `crates/orange/src/media_diagnostics.rs:647-705`
- Test: `crates/orange/src/window.rs`

**Interfaces:**
- Produces: non-cloneable `PlaybackWindow` containing one `Option<JoinHandle<()>>`.
- Produces: cloneable `PlaybackWindowHandle` containing only the HWND, overlay, liveness state, and shutdown identity needed by passive callbacks.
- Produces: `PlaybackWindow::handle(&self) -> PlaybackWindowHandle`.
- Changes: `webrtc::Output::Window(PlaybackWindowHandle)` and callback/diagnostics arguments to passive handles.

- [ ] **Step 1: Add failing owner/handle lifecycle tests**

Add Windows tests proving that dropping a passive handle does not close the window, dropping the unique owner closes and joins while a passive handle remains, and a mismatched private shutdown identity is rejected.

- [ ] **Step 2: Verify the tests fail because `PlaybackWindow` is cloneable and has no worker handle**

Run: `. .\dev.ps1; cargo test -p orange window::tests::playback_owner -- --nocapture`

- [ ] **Step 3: Split owner from handle and retain the native thread**

Change `spawn_window` to return the HWND plus its `JoinHandle`. Use one private shutdown message with a per-window identity; execute `DestroyWindow` only on the creator thread. `PlaybackWindow::drop` posts shutdown and joins without holding the overlay mutex.

- [ ] **Step 4: Keep the owner outside every GStreamer graph**

Retain `PlaybackWindow` in synchronous CLI command scope. Pass only `PlaybackWindowHandle` through `Output`, bus handlers, overlay callbacks, and diagnostics workers. Ensure host/watch/loopback/preview pipelines reach `Null` before the owner drops.

- [ ] **Step 5: Run focused playback and receive tests**

Run: `. .\dev.ps1; cargo test -p orange window::tests webrtc::tests peer::tests`

- [ ] **Step 6: Commit**

```text
git add crates/orange/src/window.rs crates/orange/src/main.rs crates/orange/src/webrtc.rs crates/orange/src/peer.rs crates/orange/src/overlay.rs crates/orange/src/media_diagnostics.rs
git commit -m "Own playback window threads"
```

### Task 3: Own Per-Viewer Startup Keyframes

**Files:**
- Modify: `crates/orange/src/peer.rs:914-1260`
- Test: `crates/orange/src/peer.rs`

**Interfaces:**
- Produces: private `StartupKeyframeWorker` stored in `ViewerBranch`.
- Produces: cloneable one-shot trigger captured by `watch_connection`.
- Preserves: immediate request plus cumulative 250 ms, 500 ms, and 750 ms retries while connected.

- [ ] **Step 1: Add failing cancellation and one-shot tests**

Test that the connection trigger starts at most once, cancellation interrupts the first timed wait, and dropping the owner joins before branch teardown continues. Use an injected request closure only inside the private worker loop; do not mock GStreamer globally.

- [ ] **Step 2: Verify cancellation fails with the detached implementation**

Run: `. .\dev.ps1; cargo test -p orange peer::tests::startup_keyframe_worker -- --nocapture`

- [ ] **Step 3: Implement the owned worker with standard channels**

Spawn the named worker while building `ViewerBranch`. The GStreamer connection callback only sends a one-shot start command. Use interruptible `recv_timeout` waits; the owner sends stop and joins.

- [ ] **Step 4: Stop the worker before GStreamer branch removal**

In `remove_viewer`, stop/join startup requests, then drop per-viewer diagnostics, then block/unlink tee pads, set `webrtcbin` to `Null`, release request pads, and remove elements.

- [ ] **Step 5: Run focused host teardown tests**

Run: `. .\dev.ps1; cargo test -p orange peer::tests`

- [ ] **Step 6: Commit**

```text
git add crates/orange/src/peer.rs
git commit -m "Own startup keyframe workers"
```

### Task 4: Own Receive-Side Monitor Threads

**Files:**
- Modify: `crates/orange/src/peer.rs:262-300,1326-1528`
- Modify: `crates/orange/src/webrtc.rs:143-220,611-676`
- Test: `crates/orange/src/peer.rs`
- Test: `crates/orange/src/webrtc.rs`

**Interfaces:**
- Produces: private incoming-bitrate worker with stop sender and `JoinHandle`.
- Produces: private audio-control worker with stop sender and `JoinHandle`.
- Produces: one receive-worker registry owned by each watch or loopback session, with workers taken from the registry before joining.

- [ ] **Step 1: Add failing interruptible-wait tests**

Test that each worker exits promptly when stopped rather than waiting one second or 50 ms, and that a registry rejects replacing an already-owned worker from a duplicate media pad.

- [ ] **Step 2: Verify the tests fail because both workers are detached**

Run: `. .\dev.ps1; cargo test -p orange receive_worker -- --nocapture`

- [ ] **Step 3: Return owners from bitrate and audio worker creation**

Replace `thread::sleep` with `recv_timeout`. Store worker owners from pad-added callbacks in the session registry; never replace or join one while holding the registry or overlay lock.

- [ ] **Step 4: Stop and join before receive graph destruction**

Signal workers when the session ends, take them out of the registry, join outside locks, drop diagnostics, then set the pipeline to `Null` and close signaling.

- [ ] **Step 5: Run receive, loopback, and audio tests**

Run: `. .\dev.ps1; cargo test -p orange webrtc::tests peer::tests`

- [ ] **Step 6: Commit**

```text
git add crates/orange/src/peer.rs crates/orange/src/webrtc.rs
git commit -m "Own receive monitor workers"
```

### Task 5: Drain And Join The Process Diagnostics Writer

**Files:**
- Modify: `crates/orange/src/media_diagnostics.rs:15-187,442-705`
- Modify: `crates/orange/src/main.rs:190-324`
- Modify: `crates/orange/src/peer.rs:602-872,1326-1528`
- Test: `crates/orange/src/media_diagnostics.rs`

**Interfaces:**
- Produces: stack-owned `DiagnosticWriter` containing the only writer `JoinHandle`.
- Produces: global non-owning `Arc<DiagnosticSink>` whose `Mutex<Option<SyncSender<String>>>` can be taken by the stack owner during shutdown.
- Removes: synchronous `flush_diagnostics` calls from async host/watch teardown.

- [ ] **Step 1: Add local writer shutdown tests**

Using a locally constructed writer and in-memory destination, test that shutdown drains accepted lines, rejects later sends, joins exactly once, and remains idempotent. Do not shut down the process-global `OnceLock` from parallel tests.

- [ ] **Step 2: Verify the tests fail because only a static sender is retained**

Run: `. .\dev.ps1; cargo test -p orange media_diagnostics::tests::diagnostic_writer_shutdown -- --nocapture`

- [ ] **Step 3: Make the stack guard own the worker and close the global sink**

Clone the sender while briefly holding its poison-recovering mutex, then release the mutex before payload serialization. Shutdown takes and drops the globally reachable sender, lets in-flight sender clones finish, drains accepted lines, flushes once at loop end, then joins the stack-owned writer handle.

- [ ] **Step 4: Put one guard in synchronous CLI scope**

Construct a lazy `DiagnosticsGuard` in `main`; guard creation must not enable diagnostics or create files. Remove bounded synchronous flushes from Tokio host/watch functions.

- [ ] **Step 5: Run all diagnostics and host/watch tests**

Run: `. .\dev.ps1; cargo test -p orange media_diagnostics::tests peer::tests`

- [ ] **Step 6: Commit**

```text
git add crates/orange/src/media_diagnostics.rs crates/orange/src/main.rs crates/orange/src/peer.rs
git commit -m "Own the diagnostics writer lifecycle"
```

### Task 6: Classify And Close Remaining Process Lifetimes

**Files:**
- Audit and modify only when its production lifetime is unowned: `crates/orange-tray/src/main.rs`, `crates/orange-tray/src/tray.rs`, `crates/orange-tray/src/update.rs`, `crates/orange-tray/src/supervisor.rs`, `crates/orange-signal/src/lib.rs`, `crates/orange/src/webrtc.rs`
- Test: affected module-local test sections

**Interfaces:**
- Produces: an explicit owner or documented application-runtime lifetime for every production `thread::spawn`, `tokio::spawn`, child process, native handle, GStreamer probe, and requested pad.
- Preserves: update checks remain non-blocking to GPUI; tray close behavior and updater handoff remain unchanged.

- [ ] **Step 1: Check the complete spawn/resource inventory**

Run: `rg "thread::spawn|thread::Builder|tokio::spawn|request_pad_simple|add_probe" crates -g "*.rs"`

For each production match, record its owner and teardown path in the commit message or architecture document. Test-only poison and helper threads are excluded.

- [ ] **Step 2: Add one failing test per remaining unowned production lifetime**

Tests must prove externally relevant teardown: task resources dropped, child reaped, native icon/window released, update worker result reaped, or requested pad/probe released. Do not add tests that merely inspect struct fields.

- [ ] **Step 3: Apply the smallest local ownership fix**

Reuse the owning feature type (`UpdateController`, tray installation owner, `SignalClient`, or receive session) rather than creating a shared worker manager. Bounded one-shot operations may be joined after their result is observed; application-runtime GPUI tasks must terminate when their entity/application context disappears.

- [ ] **Step 4: Run affected package tests**

Run: `. .\dev.ps1; cargo test --workspace --locked`

- [ ] **Step 5: Commit**

```text
git add crates
git commit -m "Close remaining process lifetimes"
```

### Task 7: Split Stable Responsibilities Without Redesign

**Files:**
- Create: `crates/orange/src/peer/host.rs`
- Create: `crates/orange/src/peer/host_branch.rs`
- Create: `crates/orange/src/peer/watch.rs`
- Create: `crates/orange/src/media_diagnostics/writer.rs`
- Create: `crates/orange/src/media_diagnostics/operation.rs`
- Create: `crates/orange/src/media_diagnostics/progress.rs`
- Create: `crates/orange/src/media_diagnostics/webrtc_monitor.rs`
- Create: `crates/orange/src/overlay/render.rs`
- Create: `crates/orange-tray/src/view.rs`
- Create: `crates/orange-signal/src/protocol.rs`
- Create: `crates/orange-signal/src/client.rs`
- Create: `crates/orange-signal/src/relay.rs`
- Create: `crates/orange-signal/src/diagnostics.rs`
- Modify: corresponding current facade files and module declarations
- Test: move existing tests with the code they characterize

**Interfaces:**
- `peer.rs` remains the facade and owns shared bus, connection-state, ICE, and SDP helpers.
- `peer::host` owns host capture/signaling orchestration; `peer::host_branch` owns tee/request-pad construction, offer creation, and deterministic viewer removal; `peer::watch` owns receive-session orchestration.
- `media_diagnostics.rs` remains the facade; `writer.rs` owns JSONL persistence, `operation.rs` owns timed operation events, `progress.rs` owns stage counters/probes, and `webrtc_monitor.rs` owns WebRTC stats polling.
- `overlay.rs` owns interaction state and GStreamer attachment; `render.rs` owns rasterization.
- tray `main.rs` owns `Orange` state/effects/bootstrap; `view.rs` owns the existing Render implementation, global chrome, listener wiring, and all six screen methods in one child module.
- `orange-signal::protocol` owns `Signal`; `client` owns `SignalClient` and connection tasks; `relay` owns room/peer state; `diagnostics` owns upload storage, metadata validation, and its route handler. Existing public re-exports remain stable.

- [ ] **Step 1: Record baseline test names and file sizes**

Run: `cargo test --workspace --locked -- --list`

Run: `Get-ChildItem crates -Recurse -Filter '*.rs' | ForEach-Object { "{0,5} {1}" -f (Get-Content $_.FullName).Count, $_.FullName }`

- [ ] **Step 2: Move one responsibility at a time**

Move function/type bodies verbatim, then use only `pub(super)` or `pub(crate)` needed by the parent facade. Do not rename behavior, introduce traits, group unrelated arguments, rewrite callbacks during file moves, or create one tray file per screen.

- [ ] **Step 3: Run the owning package after each move**

Run the exact package test suite after each extraction. A move is complete only when test names/counts match the baseline and strict Clippy remains clean.

- [ ] **Step 4: Verify dependency direction**

The feature facade may call child modules. Child modules must not call a sibling through the facade when a direct sibling import suffices, and no `orange::webrtc -> orange::peer` cycle may return.

- [ ] **Step 5: Commit each mechanical extraction separately**

Use one commit per stable boundary, for example `Split viewer branch ownership from peer sessions` and `Split tray screens from application effects`.

### Task 8: Add A Newcomer Architecture Map And Correct Stale Guidance

**Files:**
- Create: `ARCHITECTURE.md`
- Modify: `README.md`
- Modify: module-level documentation in extracted facades

**Interfaces:**
- Produces: one repository map naming each crate, authoritative policy module, process/thread boundary, data flow, and exact files to change for common tasks.
- Produces: a verification/release section that distinguishes local development, package validation, installed smoke tests, publication, and signing.

- [ ] **Step 1: Write the architecture map from the final module graph**

Include sections for binaries/crates, host media flow, watch media flow, signaling/auth flow, ownership/teardown rules, release/update flow, and a “Where do I change…?” table for codec/RTP policy, capture, receive graph, overlay, tray screens/state, relay protocol, auth, diagnostics, installer, and deployment.

- [ ] **Step 2: Correct stale README claims**

Replace the obsolete “one machine is the next test,” AV1/NVENC production diagrams, unlimited-relay wording, contradictory no-account wording, and stale session-storage statements with the validated H.265 path, fixed process-local limits, and explicit one-replica model. Keep AV1 labeled as a diagnostic/development option.

- [ ] **Step 3: Verify every documented path and command**

Run: `git ls-files` and check every path in `ARCHITECTURE.md` exists. Run every local verification command listed in README/architecture except external deploy, publish, signing, and two-machine hardware acceptance.

- [ ] **Step 4: Commit**

```text
git add ARCHITECTURE.md README.md crates/*/src
git commit -m "Document Orange architecture and change paths"
```

### Task 9: Final Review And Acceptance Gates

**Files:**
- Modify only if review finds a defect.

**Interfaces:**
- Produces: one reviewed exact commit suitable for final installed acceptance.

- [ ] **Step 1: Run the complete locked gate**

Run: `. .\dev.ps1; cargo test --workspace --all-features --locked`

Run: `. .\dev.ps1; cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`

Run: `cargo fmt --all -- --check`

Run: `. .\dev.ps1; cargo build --release --workspace --all-features --locked`

- [ ] **Step 2: Run all PowerShell validation suites**

Run each test script directly so `$LASTEXITCODE` cannot leak between scripts: package provenance, installer, beta publisher, Azure deployment, alpha publisher, and alpha launcher.

- [ ] **Step 3: Build the exact-commit installer**

Run: `$build = (git rev-parse HEAD).Trim(); .\package.ps1 -BuildId $build`

Record installer size, SHA-256, and Authenticode status.

- [ ] **Step 4: Independently review every cleanup commit**

Review lifecycle safety, GStreamer/Win32 teardown ordering, protocol compatibility, test honesty, module visibility, dependency direction, and over-engineering. Fix every Critical, High, or Important finding and repeat the full gate.

- [ ] **Step 5: Run installed-path acceptance**

After explicit installation approval, validate preview close, H.265 1280x720@60 loopback, host/watch audio, late join, multiple viewers, updater handoff, and rollback. Docker execution, two-machine WAN acceptance, Authenticode signing, and multi-replica state remain external gates unless the required environment is available.
