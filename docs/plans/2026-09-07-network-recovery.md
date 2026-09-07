# Network diagnostics and recovery implementation

Spec: `docs/specs/2026-09-07-network-recovery.md`.

Base: fetched origin/main f7b6884 (1.0.5). Clean serial baseline: 464 passed,
five ignored. Worktree: `orange-network-recovery`.

## Implementation

- Add actual candidate lifecycle events, selected-route metadata and native
  GStreamer version, with owned per-connection trackers and a saturating cap.
- Add an isolated receiving ICE probe using the same STUN setting as streaming.
  Exercise local host gathering and a local fake STUN server in tests.
- Extend the CLI to schema 2's nine checks; retain schema 1 compatibility in
  the desktop. Export only finite network metadata values and numeric fields.
- Add one supervised fresh child retry for initial ICE failure. Reap the old
  child and its readers first; stop after one retry, and exclude clean exit,
  stream ending, post-connect failures and sign-out.
- Keep latest historical connection evidence separate from current checks.
  Offer a targeted Windows repair only when inspection identifies an eligible
  application rule; record the repair outcome and verify settings afterward.
- Scope elevated work to the media executable and local active-profile policy.
  Keep the helper's diagnostics writer disabled and use fixed exit codes.

## Verification

- Serial workspace tests with `--locked --all-features`: orange 275 passed
  (3 ignored), client 172 passed (2 ignored), signal 81 passed, updater 9
  passed. Clippy `-D warnings` all targets/features and `cargo fmt --all
  -- --check` passed.
- Firewall mock harness: 24 scenarios passed against production PowerShell
  functions. No real firewall mutation. Bounded-child tests use a fixture
  binary, not `powershell.exe`.
- Live `orange troubleshoot` on this machine against production signalling:
  schema 2, ice `Complete` with 15 candidates (5 UDP, 10 TCP, 6 IPv4, 9 IPv6,
  12 host, 3 server-reflexive), firewall pass with no matching inbound block.
  Gathering only; friend connectivity untested.
- `orange repair-network --elevated --requester 0` exited 6 before inspection
  or mutation.
- Native Settings fixture (outside the repository) exercised Fix connection
  with a fake child, then an automatic recheck. Last connection stayed red
  while current Windows settings turned green. Space on the focused Run
  control completed exactly one recheck after a duplicate key-handler was
  removed. Screenshots:
  `%LOCALAPPDATA%\Temp\opencode\orange-network-recovery-screens`.

Real firewall mutation and the affected friend's network were not exercised.
