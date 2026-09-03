# Immediate Viewer Connection Experience Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Reveal the existing playback HWND immediately and show truthful, monotonic connection feedback until the first decoded video frame arrives.

**Architecture:** Add a small connection reducer shared by watch signaling, WebRTC callbacks, the native painter, and the video overlay. `PlaybackWindowHandle` owns the thread-safe transition boundary: callbacks update pure state and post a private window message, while only the HWND thread invalidates or paints. A one-shot probe on the decoder source marks `Connected`, after which D3D11 presents into the same HWND without native repaint or recreation.

**Tech Stack:** Rust 1.98, Tokio, GStreamer/webrtcbin 0.25, windows-rs 0.62 Win32/GDI, existing Orange diagnostics and overlay infrastructure

**Spec:** `docs/superpowers/specs/2026-09-03-immediate-viewer-connection-experience.md`

## Global Constraints

- Use the existing playback HWND and D3D11 receive pipeline; add no dependency or temporary window.
- Keep every HWND mutation on the creator thread via posted messages.
- Keep stage labels and failure copy static and privacy-safe.
- Preserve existing media transport, codec, ICE, STUN/TURN, and signaling behavior.
- Add no artificial delay.
- Preserve owner-before-pipeline declaration and worker/pipeline-before-HWND teardown ordering.

---

### Task 1: Monotonic connection model

**Files:**
- Create: `crates/orange/src/connection.rs`
- Modify: `crates/orange/src/main.rs:3-15`
- Test: `crates/orange/src/connection.rs`

**Interfaces:**
- Produces: `ConnectionEvent`, `ConnectionFailure`, `ConnectionStage`, `ConnectionTracker::begin`, `ConnectionTracker::advance`, and `ConnectionTracker::snapshot`.
- Produces: centralized `primary`, `title`, `detail`, and diagnostic labels on `ConnectionStage`.

- [ ] **Step 1: Write failing reducer tests**

```rust
#[test]
fn real_events_map_to_the_six_user_facing_stages() {
    let mut model = ConnectionModel::default();
    assert_eq!(model.stage(), ConnectionStage::JoiningRoom);
    for (event, expected) in [
        (ConnectionEvent::StreamInfo, ConnectionStage::ExchangingStreamDetails),
        (ConnectionEvent::IceChecking, ConnectionStage::FindingDirectRoute),
        (ConnectionEvent::IceConnected, ConnectionStage::SecuringConnection),
        (ConnectionEvent::PeerConnected, ConnectionStage::StartingMedia),
        (ConnectionEvent::FirstVideoFrame, ConnectionStage::Connected),
    ] {
        model.advance(event);
        assert_eq!(model.stage(), expected);
    }
}
```

Add separate tests proving late SDP/pad events cannot move backward and a `Failed(ConnectionFailure::Network)` state cannot be overwritten.

- [ ] **Step 2: Run the new tests and confirm the module/API is missing**

Run: `cargo test -p orange --bin orange connection::tests -- --nocapture`

Expected: compilation fails because `connection` is not yet declared or its types are absent.

- [ ] **Step 3: Implement the minimal reducer and tracker**

Use an ordinal only for the five progressive states. Treat both `Connected` and `Failed(_)` as terminal. `PadAdded` and `ReceiveBranchReady` map only to `ExchangingStreamDetails`, so early transceiver creation cannot falsely claim that media is starting.

- [ ] **Step 4: Run reducer tests**

Run: `cargo test -p orange --bin orange connection::tests -- --nocapture`

Expected: all connection model tests pass.

### Task 2: Existing-HWND connection surface

**Files:**
- Modify: `crates/orange/src/window.rs:101-452`
- Modify: `crates/orange/src/window/native.rs:1-1164`
- Modify: `crates/orange/src/overlay.rs:66-359`
- Test: `crates/orange/src/window.rs`
- Test: `crates/orange/src/window/native.rs`

**Interfaces:**
- Consumes: `ConnectionTracker`, `ConnectionEvent`, and `ConnectionStage` from Task 1.
- Produces: `PlaybackWindowHandle::begin_connection()` and `PlaybackWindowHandle::connection_event(ConnectionEvent)`.
- Produces: private `CONNECTION_MESSAGE`, handled only by the HWND thread.

- [ ] **Step 1: Write failing native lifecycle tests**

Add tests that begin connection feedback, reveal the HWND with no sink, assert it becomes visible and responsive, close it at each progressive stage, and assert the HWND value remains unchanged across a simulated media-ready transition.

- [ ] **Step 2: Run window tests and confirm the new handle methods are absent**

Run: `cargo test -p orange --bin orange window::tests -- --nocapture --test-threads=1`

Expected: compilation fails on `begin_connection` / `connection_event`.

- [ ] **Step 3: Share the tracker and marshal updates**

Construct one `Arc<ConnectionTracker>` beside `OverlayState`, store it in the passive handle and native `WindowContext`, and post `CONNECTION_MESSAGE` after accepted transitions. Emit `connection-stage` diagnostics containing only stable stage/event names, total elapsed milliseconds, and milliseconds spent in the prior stage.

- [ ] **Step 4: Paint connection copy in `WM_PAINT`**

Use the registered dark background, GDI Segoe UI fonts, cream text, and a small orange accent. Draw `Connecting...`, the stage title, and its detail while the tracker is active and pre-video; draw actionable failure copy for `Failed`. On `Connected`, validate paint without drawing so the D3D11 frame replaces existing pixels. Invalidate on stage changes and resize only from `wnd_proc`.

- [ ] **Step 5: Make overlay labels use the same stage snapshot**

When connection tracking is active but not connected, derive `quality_label` and `detail_label` from `ConnectionStage`; otherwise retain the existing resolution/FPS/host labels.

- [ ] **Step 6: Run lifecycle and overlay tests**

Run: `cargo test -p orange --bin orange window:: overlay:: -- --nocapture --test-threads=1`

Expected: all selected tests pass and no HWND is recreated.

### Task 3: Real watch and first-frame events

**Files:**
- Modify: `crates/orange/src/peer.rs:63-189`
- Modify: `crates/orange/src/peer/watch.rs:101-427`
- Modify: `crates/orange/src/webrtc/receive.rs:260-398`
- Modify: `crates/orange/src/webrtc.rs:263-307`
- Modify: `crates/orange/src/main.rs:347-368`
- Test: `crates/orange/src/webrtc/receive.rs`
- Test: `crates/orange/src/peer/watch.rs`

**Interfaces:**
- Consumes: `PlaybackWindowHandle::begin_connection` and `connection_event`.
- Produces: one-shot decoded-buffer probe that sends `ConnectionEvent::FirstVideoFrame` and removes itself.

- [ ] **Step 1: Write failing event/probe tests**

Test that a buffer probe fires once and preserves both buffers, and that beginning a watch reveals the window independently of sink attachment. Add a title helper test proving the room code is not used as visible window text.

- [ ] **Step 2: Run targeted tests and verify the missing behavior fails**

Run: `cargo test -p orange --bin orange connection window webrtc::receive::tests peer::watch::tests -- --nocapture --test-threads=1`

Expected: new assertions fail before watch/event wiring exists.

- [ ] **Step 3: Reveal before asynchronous negotiation**

Call `begin_connection()` and `reveal()` at the start of windowed `run_watch`, before awaiting signaling. Replace the room-code window title with a generic Orange viewer title.

- [ ] **Step 4: Wire truthful progress events**

Feed StreamInfo and SDP callbacks into `ExchangingStreamDetails`; ICE gathering/checking into `FindingDirectRoute`; ICE connected/completed into `SecuringConnection`; peer connected into `StartingMedia`; and pad/branch events into the lower-bound reducer events. Promise callbacks use cloned passive handles and never call Win32 directly.

- [ ] **Step 5: Wire privacy-safe failures**

Map signaling startup/channel errors, room rejection, SDP failures, terminal peer state, receive branch failure, and pipeline startup/bus errors to fixed `ConnectionFailure` variants before preserving the existing error return and cleanup flow. Never place raw server, SDP, candidate, or room data in connection copy or stage diagnostics.

- [ ] **Step 6: Transition on decoded media and preserve loopback reveal**

Install a one-shot `BUFFER` probe on the decoder source for window output. Remove the old reveal from `build_receive_branch`; reveal explicitly after successful loopback branch construction so diagnostic behavior remains unchanged.

- [ ] **Step 7: Run all Orange tests**

Run: `cargo test -p orange --bin orange -- --test-threads=1`

Expected: all automated Orange tests pass; hardware-only tests remain ignored.

### Task 4: Verification and manual matrix

**Files:**
- Modify only if verification exposes a defect in the files above.

**Interfaces:**
- Consumes: completed Tasks 1-3.
- Produces: formatting, lint, lifecycle, workspace, and available manual evidence.

- [ ] **Step 1: Format and check**

Run: `cargo fmt --all -- --check`

Run: `cargo clippy --workspace --all-targets -- -D warnings`

- [ ] **Step 2: Run preview/window lifecycle tests**

Run: `cargo test -p orange --bin orange window:: -- --nocapture --test-threads=1`

Run where an interactive desktop is available: `cargo run -p orange -- preview --size 1920x1080 --window 1280x720 --pattern smpte --pin`

- [ ] **Step 3: Run all workspace tests**

Run: `cargo test --workspace -- --test-threads=1`

Expected baseline: 256 passing, 3 ignored.

- [ ] **Step 4: Exercise peer timing paths when peers are available**

Verify a fast peer (~500 ms) and the known slow peer (~2.2 seconds in ICE checking). For both, confirm immediate dark-window feedback, truthful stage progression without delay, clean first-frame replacement, and prompt close/teardown at each observable stage. Record unavailable peer/network combinations explicitly rather than claiming them.

- [ ] **Step 5: Review the final diff**

Run: `git diff --check`

Run: `git status --short`

Confirm there are no transport, STUN/TURN, signaling protocol, codec, or unrelated branding changes.
