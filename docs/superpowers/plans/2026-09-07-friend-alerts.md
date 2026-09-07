# Responsive Friend Presence and Alerts Implementation Plan

**Goal:** Remove the 15-second discovery wait, play a short friend-start cue with per-friend mute, and collapse the friend-management panel shown in the request.

**Architecture:** Extend the existing authenticated presence HTTP request with an optional bounded wait and snapshot revision. Waiting checks in-memory room state, releases the scarce social I/O permit, and reauthorizes before returning changed state. Reuse the client's owned HTTP jobs, preferences and synthesized sound player.

**Tech stack:** Rust, Axum/Tokio, reqwest blocking worker, GPUI, existing Win32 sound playback.

**Spec:** User request and two screenshots in this session. Collapsing management leaves stream rows available; incoming requests remain discoverable. Mutes persist per signed-in account on this device.

## Constraints

- No new dependencies. Legacy presence requests and older relay replies remain compatible.
- Keep authentication, mutual friendship authorization, removal tombstones, bounded request admission, cancellation and worker ownership.
- Never alert for initial presence, unchanged live state, or live/full capacity transitions. Network failures must not manufacture a stream start.
- Reuse existing UI controls and focus traversal. Remember collapse state across restarts.

## Tasks

- [x] Verify clean workspace baseline with `cargo test --locked --workspace` after dot-sourcing `dev.ps1`. Parallel run hit four documented media child deadlines; serial rerun passed 414 tests, 5 ignored.
- [x] Add relay behavior tests for a pending presence request returning after host start/stop, privacy after removal, bounded waiting, and legacy JSON compatibility; run the targeted tests red (missing revision/wait behavior).
- [x] Implement optional `wait=20` and `since=<revision>` on `/presence`; opted-in replies add `revision`. Wait at most 20 seconds, with 250 ms in-memory checks, a separate 512-waiter limit, and no social permit or database request while idle. Reacquire admission and recheck identity/relationships before returning. Signal package: 63 tests passed.
- [x] Add client checks for wait/revision transport, legacy fallback, cancellation, first-snapshot suppression, stream transitions, and account-specific mute persistence. Client package: 134 passed, 2 ignored.
- [x] Renew successful revision-bearing presence requests immediately; retain interval backoff on failure and legacy replies. Add one coalesced short cue for newly live unmuted friends, using retained observed presence across transient failures.
- [x] Add per-account mute IDs and persisted management collapse preference. Native keyboard testing caught duplicate key/click activation; removing the explicit key handlers fixed it. The saved-preference native regression and titlebar drag checks pass.
- [x] Run client and signal tests, then workspace all-feature tests, clippy with warnings denied, and formatting check. Final all-feature serial suite: 432 passed, 5 ignored; workspace all-target/all-feature clippy and formatting pass. Relay review and native UI findings are fixed and regression-tested.
- [x] Update architecture and README with the implemented protocol and controls, and record verification results.

Review follow-up: post-wait presence refreshes now queue for up to five seconds
for one of the 32 I/O slots rather than immediately returning 429 on simultaneous
wakeups. Real HTTP contention and 512-waiter capacity tests pass (66 signal tests).

Final client review found no blocker in presence renewal, bounded workers,
cue gating or mute persistence. Its keyboard coverage concern is covered by
the native fixture regression added here. Both client and relay development
binaries build successfully. Deployment was subsequently requested: deploy the
relay, publish the desktop release, and refresh the website download snapshot.
