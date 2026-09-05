# Six performance improvements

## Scope

Implement the six source-review findings approved by the user on 2026-09-04.
Do not change transport queue budgets, encoder settings, audio latency, relay
admission policy, dependencies, packaging, or release versions.

Baseline: `aac1fb7`, `cargo test --locked --workspace`: 308 passed, 3 ignored.

## Work and verification

- [x] Host idle lifecycle (`peer/host.rs`, `peer/host_branch.rs`): stop the
  shared pipeline after the final branch detaches, restart on the next join,
  and test teardown/new-join ordering and the 1 -> 0 -> 1 transition.
- [x] Client animation (`ui/controls.rs`, `view/home.rs`, `view/stream.rs`):
  pass the existing active-window flag into every live dot. Test that the
  inactive branch does not construct a repeating animation.
- [x] Overlay (`overlay.rs`, `overlay/raster.rs`): key composition reuse from
  raw state without formatting labels; cache bounded opaque icon artwork and
  apply opacity at composition. Test invalidation, hidden reuse, artwork reuse,
  and alpha/DPI correctness.
- [x] Client jobs (`background.rs`, `main.rs`, `supervisor.rs`): enumerate
  windows off-thread; cancel without joining on UI actions; retain and reap
  owners, coalesce replacement, reject stale results, join on final shutdown.
  Barrier-controlled tests must prove cancellation returns while work is blocked
  and replacements never accumulate concurrent workers.
- [x] Client HTTP (`background.rs`, `main.rs`, `presence.rs`): back off failed
  avatar requests, reuse a client per avatar batch and across presence polls,
  preserve timeouts/body limits. Test retry deadlines, stale URL results and
  local keep-alive reuse without external network calls.
- [x] Relay auth (`orange-signal/src/auth.rs`): share bounded memory-cache
  insertion between login and durable restoration; preserve timestamps and
  replacement, evict memory only. Test full/overfull caches and concurrent
  insertions without involving Azure.

For each task, reproduce the regression with a failing test before its fix.
Run focused tests after each change. After integration run workspace tests with
all features, Clippy for all targets/features with warnings denied, and fmt
check. Review lifecycle changes independently before completion.

Interactive acceptance remains separate: host GPU/CPU at zero viewers,
hardware capture/audio resume, unfocused client redraw counts, picker response,
and viewer CPU/allocations. Automated tests do not establish measured FPS or
power savings. No deployment or release is part of this implementation.

## Focused evidence

- Relay overfull-cache regression failed at 4,099 entries before the fix and
  now stays at 4,096. All 50 signal-crate tests pass.
- Combined media changes pass 200 tests with one existing hardware test ignored.
  Synthetic video/audio progress stops after the final branch detaches and
  resumes after a new branch is attached.
- Optimized builds of `orange` and `orange-relay` pass; these are local builds,
  not packaged or published releases.
- Counters at actual overlay label/raster work sites: 600 hidden or expanded
  cache hits went from 1,200 label builds to zero; 100 alpha variants went from
  100 icon rasterizations to one; 20 warmed pulse redraws went from 60 icon
  rasterizations to zero. Opaque icons match the previous pixels exactly;
  faded edges differ by at most two 8-bit channel levels in the regression set.
- Real loopback HTTP tests confirm connection reuse across an avatar batch and
  across presence polls, while keeping bearer tokens per request. Scheduler
  tests hold workers behind barriers to check prompt cancellation, ownership,
  coalescing and rejection of queued stale results.
- Independent review found a stale-picker integration issue: a new scan could
  leave old HWNDs selectable until it returned. The UI caches are now cleared
  before discovery starts; a populated-cache regression reproduced the issue.
  Follow-up review verified the correction. Host, overlay and auth/animation
  reviews found no blocking issues. Auth tests exercise the shared cache helper,
  not a fake Azure-backed `identify` round trip; that remains a coverage limit.

## Final source gates

- `cargo test --locked --workspace --all-features`: 354 passed, 3 existing
  ignored tests, no failures (200 media, 95 client, 50 signal, 9 updater).
- `cargo clippy --locked --workspace --all-targets --all-features -- -D warnings`:
  passed.
- `cargo fmt --all -- --check`: passed.
- `cargo build --locked --release --workspace --all-features`: passed after
  resuming the initial build that exceeded the tool's two-minute timeout.
- No Cargo dependencies, versions, packaging or deployment files changed.
