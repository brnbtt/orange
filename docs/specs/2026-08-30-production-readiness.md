# Orange Production Readiness

## Objective

Make the beta codebase safe to evolve without changing the validated product behavior or adding features.

## Frozen Behavior

- H.265 remains the production video path: `mfh265enc` to `rtph265pay` / `rtph265depay` to `d3d11h265dec`.
- Opus remains payload type 111; primary video remains 96 and video RTX remains 97.
- Live receive latency remains 100 ms with `do-lost=true`, audio `drop-on-latency=false`, and video `drop-on-latency=true`.
- `ORANGE_RTP_BUFFER_MODE=none` remains supported and sets `buffer-mode=none` and `rtcp-sync=never`.
- The host captures and encodes once while maintaining independent RTP/WebRTC branches per viewer.
- Process-scoped game audio and whole-system screen audio retain their current semantics.
- Installer, update, diagnostics, and relay wire formats remain compatible with beta.5.

## Workstreams

1. **Media boundaries:** centralize RTP policy, capture construction, receive graph construction, and viewer branch ownership behind characterization tests.
2. **Tray architecture:** separate application state transitions and effects from GPUI rendering, then split screens and components.
3. **Native ownership:** replace unconstrained Win32 pointers and duplicated GDI cleanup with scoped accessors and RAII; guarantee non-null GLib callback returns.
4. **Persistence and updates:** return contextual errors, write atomically, and share the updater handoff contract and checksum implementation.
5. **Relay scale and authentication:** expire sessions, enforce connection/rate budgets, add soak/load tests, and define the multi-replica state model.
6. **Release reproducibility:** pin the Rust toolchain, make all artifacts immutable, separate release storage from relay teardown, and consolidate provenance checks.
7. **Operational readiness:** establish CI gates, structured sanitized logging, resource budgets, health checks, failure drills, and rollback verification.

## Definition Of Done

- Every workstream lands as independently reviewed, behavior-preserving commits.
- `cargo fmt --all -- --check`, strict workspace Clippy, all workspace tests, release builds, packaging tests, and installer tests pass in CI.
- Media characterization includes H.265 host/watch, loopback, file output, late join, multiple viewers, clean audio, loss recovery, teardown, and the `ORANGE_RTP_BUFFER_MODE=none` override with `buffer-mode=none` and `rtcp-sync=never`.
- Relay tests cover bounded slow consumers, repeated role attempts, authentication expiry, connection floods, and sustained room churn.
- Unsafe blocks have explicit invariants and are contained behind owned safe interfaces.
- No production child process, thread, async task, native handle, requested pad, or temporary update artifact has an unowned lifecycle.
- Release artifacts are reproducible, hash-verified, immutable, and independently recoverable from relay infrastructure.

## Delivery Rule

No feature work is mixed into these workstreams. If a cleanup requires an observable behavior change, document and approve it separately before implementation.
