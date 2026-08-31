# Media Transport Boundary Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Centralize RTP payload assignments and live receive transport policy so the beta.5 audio fix cannot drift while `peer.rs` and `webrtc.rs` are decomposed.

**Architecture:** Create `webrtc/transport.rs` as the single owner of RTP constants, generated caps, RTP packet classification, jitterbuffer policy, and live `webrtcbin` receive configuration. Keep orchestration in `peer.rs` and media graph construction in `webrtc.rs`, re-exporting only the transport functions they consume.

**Tech Stack:** Rust 2021, GStreamer 1.26 Rust bindings, `webrtcbin`, `rtpbin`, `rtpjitterbuffer`, Cargo tests, Clippy.

**Spec:** `docs/superpowers/specs/2026-08-30-production-readiness.md`

## Global Constraints

- Do not change H.265 encoder, payloader, depayloader, decoder, quality, or negotiation behavior.
- Opus payload is 111, primary video payload is 96, and video RTX payload is 97.
- Live receive latency is 100 ms with `do-lost=true`, audio `drop-on-latency=false`, and video `drop-on-latency=true`.
- Preserve `ORANGE_RTP_BUFFER_MODE=none` behavior.
- Do not add dependencies or public APIs.
- Run `. .\dev.ps1` before media builds and tests.

---

### Task 1: Centralize RTP Constants And Caps

**Files:**
- Create: `crates/orange/src/webrtc/transport.rs`
- Modify: `crates/orange/src/webrtc.rs:15-69`
- Test: `crates/orange/src/webrtc/transport.rs`

**Interfaces:**
- Produces: `pub(super) const VIDEO_PAYLOAD: i32`, `VIDEO_RTX_PAYLOAD: i32`, and `AUDIO_PAYLOAD: i32`.
- Produces: `pub(crate) fn video_rtp_caps(codec: Codec, frame_rate: u32) -> gst::Caps`.
- Produces: `pub(crate) fn audio_rtp_caps() -> gst::Caps`.

- [x] **Step 1: Add failing transport contract tests**

```rust
#[test]
fn generated_caps_use_non_overlapping_payload_assignments() {
    gst::init().unwrap();
    for codec in [Codec::Av1, Codec::H265, Codec::H264] {
        assert_eq!(payload(&video_rtp_caps(codec, 60)), VIDEO_PAYLOAD);
    }
    assert_eq!(payload(&audio_rtp_caps()), AUDIO_PAYLOAD);
    assert_ne!(AUDIO_PAYLOAD, VIDEO_PAYLOAD);
    assert_ne!(AUDIO_PAYLOAD, VIDEO_RTX_PAYLOAD);
}
```

- [x] **Step 2: Run the contract test and verify it fails to compile because the transport module does not exist**

Run: `. .\dev.ps1; cargo test -p orange webrtc::transport::tests::generated_caps_use_non_overlapping_payload_assignments`

- [x] **Step 3: Move caps construction and introduce constants**

```rust
pub(super) const VIDEO_PAYLOAD: i32 = 96;
pub(super) const VIDEO_RTX_PAYLOAD: i32 = 97;
pub(super) const AUDIO_PAYLOAD: i32 = 111;

pub(crate) fn video_rtp_caps(codec: Codec, frame_rate: u32) -> gst::Caps {
    let builder = gst::Caps::builder("application/x-rtp")
        .field("media", "video")
        .field("encoding-name", codec.rtp_encoding())
        .field("payload", VIDEO_PAYLOAD)
        .field("clock-rate", 90_000i32)
        .field("a-framerate", frame_rate.to_string());
    match codec {
        Codec::H264 => builder.field("packetization-mode", "1").build(),
        Codec::Av1 | Codec::H265 => builder.build(),
    }
}
```

- [x] **Step 4: Re-export caps internally and replace numeric assertions with constants**

```rust
mod transport;
pub(crate) use transport::{audio_rtp_caps, video_rtp_caps as rtp_caps};
```

- [x] **Step 5: Run focused transport and existing WebRTC tests**

Run: `. .\dev.ps1; cargo test -p orange webrtc::`

- [x] **Step 6: Commit**

```text
git add crates/orange/src/webrtc.rs crates/orange/src/webrtc/transport.rs
git commit -m "Centralize RTP payload contracts"
```

### Task 2: Move Receiver Jitter Policy Out Of Peer Orchestration

**Files:**
- Modify: `crates/orange/src/webrtc/transport.rs`
- Modify: `crates/orange/src/webrtc.rs:190-210`
- Modify: `crates/orange/src/peer.rs:90-206,611-655,1306-1330`
- Test: `crates/orange/src/webrtc/transport.rs`

**Interfaces:**
- Consumes: payload constants and caps from Task 1.
- Produces: `pub(crate) fn configure_receive_transport(bin: &gst::Element, live_output: bool) -> Result<()>`.
- Keeps packet parsing and jitterbuffer configuration private to `webrtc::transport`.

- [x] **Step 1: Add caps-to-policy characterization tests using generated caps**

```rust
#[test]
fn generated_caps_select_the_expected_jitterbuffer_policy() {
    gst::init().unwrap();
    for codec in [Codec::Av1, Codec::H265, Codec::H264] {
        let jitter = jitterbuffer();
        assert!(configure_jitterbuffer_for_caps(
            &jitter,
            video_rtp_caps(codec, 60).as_ref(),
        ));
        assert!(jitter.property::<bool>("drop-on-latency"));
    }
    let audio = jitterbuffer();
    assert!(configure_jitterbuffer_for_caps(
        &audio,
        audio_rtp_caps().as_ref(),
    ));
    assert!(!audio.property::<bool>("drop-on-latency"));
}
```

- [x] **Step 2: Verify the new test fails before the policy is moved**

Run: `. .\dev.ps1; cargo test -p orange webrtc::transport::tests::generated_caps_select_the_expected_jitterbuffer_policy`

- [x] **Step 3: Move receive configuration and private classifiers verbatim from `peer.rs`**

Move `configure_receive_transport`, `prepare_media_jitterbuffer`, `rtp_payload_type`, `configure_jitterbuffer_for_payload`, and `configure_jitterbuffer_for_caps`. Replace numeric payload matches with `AUDIO_PAYLOAD`, `VIDEO_PAYLOAD`, and `VIDEO_RTX_PAYLOAD` converted once to `u8` constants.

- [x] **Step 4: Reverse the module dependency**

```rust
// webrtc.rs loopback
configure_receive_transport(&recv_bin, matches!(&output, Output::Window(_)))?;

// peer.rs watch orchestration
use crate::webrtc::configure_receive_transport;
```

Delete the `crate::peer::configure_receive_transport` call so `webrtc` no longer depends on `peer`.

- [x] **Step 5: Move the beta.5 receiver policy tests from `peer.rs` into `transport.rs`**

Assert `webrtcbin` and internal `rtpbin` latency are 100 ms, global drops are disabled, Opus enables GAP/PLC signaling, video stays at the live edge, RTP v2 parsing masks the marker bit, and invalid RTP is not classified.

- [x] **Step 6: Run focused tests**

Run: `. .\dev.ps1; cargo test -p orange webrtc::transport::tests`

- [x] **Step 7: Run the full Orange crate tests and strict Clippy**

Run: `. .\dev.ps1; cargo test -p orange`

Run: `. .\dev.ps1; cargo clippy -p orange --all-targets --all-features -- -D warnings`

- [x] **Step 8: Commit**

```text
git add crates/orange/src/webrtc.rs crates/orange/src/webrtc/transport.rs crates/orange/src/peer.rs
git commit -m "Isolate receiver transport policy"
```

### Task 3: Verify The Extraction Against Production Gates

**Files:**
- Modify: `docs/superpowers/plans/2026-08-30-media-transport-boundary.md`

**Interfaces:**
- Consumes: completed transport boundary from Tasks 1 and 2.
- Produces: a reviewed, behavior-preserving foundation for later receive-graph and host-branch extraction.

- [x] **Step 1: Verify the dependency cycle is absent**

Run: `rg "crate::peer::configure_receive_transport|fn configure_receive_transport" crates/orange/src`

Expected: one function definition in `webrtc/transport.rs` and no `webrtc -> peer` call.

- [x] **Step 2: Run workspace verification**

Run: `. .\dev.ps1; cargo test --workspace --all-features`

Run: `. .\dev.ps1; cargo clippy --workspace --all-targets --all-features -- -D warnings`

Run: `. .\dev.ps1; cargo build --release --workspace --all-features`

- [x] **Step 3: Run formatting checks without modifying user-owned work**

Run: `rustfmt --edition 2021 --check --config skip_children=true crates/orange/src/webrtc.rs crates/orange/src/webrtc/transport.rs crates/orange/src/peer.rs`

Run: `git diff --check`

- [x] **Step 4: Prepare the branch for independent code review**

Record the exact verification results in the implementation report and leave the branch clean except for the planned documentation commit. The controller will review the complete diff against the frozen behavior in `docs/superpowers/specs/2026-08-30-production-readiness.md`, fix every Critical, High, and Important finding, and rerun Step 2.

- [x] **Step 5: Commit plan completion**

```text
git add docs/superpowers/specs/2026-08-30-production-readiness.md docs/superpowers/plans/2026-08-30-media-transport-boundary.md
git commit -m "Document production readiness gates"
```
