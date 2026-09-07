# Networking diagnostics and targeted recovery

Base: origin/main f7b6884, Orange 1.0.5. Baseline serial workspace tests passed:
464 passed, five ignored.

The first uploaded failing report contained completed ICE gathering followed
by failed connectivity after about ten seconds and no incoming media. All
standalone checks passed. Another viewer reached the same hosting session.
The reports establish the failing layer but not a firewall or NAT cause.

## Goals

- Record actual ICE candidate lifecycle and selected-route evidence, with
  bounded fixed-value metadata that survives Send report privacy filtering.
- Extend Troubleshoot with an actual isolated candidate-gathering check and
  Windows application firewall inspection. Keep simple red/green results and
  helpful text; technical evidence belongs in the report.
- Show the latest recorded connection outcome separately from current checks,
  so successful STUN cannot conceal a recently failed streaming attempt.
- Automatically retry an initial failed peer connection once with freshly
  created connection state; show recovery progress and stop after one retry.
- Offer targeted repair for a detected, locally repairable Orange firewall
  condition, respecting Windows' required administrator permission. Never
  describe changing a setting as proof that a friend is now reachable.

## Diagnostic contract

CLI troubleshoot schema 2 adds required check IDs `ice` and `firewall` to the
existing seven. Each check retains id/status/detail; an optional `repairable`
boolean is allowed only for a failed firewall check. Detail remains bounded to
240 characters with fixed output, never raw OS errors or addresses. The new
desktop accepts schema 1's seven checks as a legacy result and schema 2's nine
checks with strict ID/count/status validation.

The ICE gathering probe creates an isolated receiving peer with a bounded
five-second deadline and no capture/playback. Count candidate types, family and
transport; gathered addresses are not proof of usable Internet paths. Always
release the probe on completion/failure/timeout. Existing child supervision
provides outer cancellation and output limits.

Live diagnostics:
- `ice-candidate`: fixed direction/action, kind, transport, address family,
  address scope, media-line index. Bound parsing and emitted records. Submission
  completion is not proof of candidate-pair validation.
- `ice-route`: selected-pair presence and sanitized local/remote descriptors
  resolved from actual stats. Do not invent missing failed-check counters.
- `ice-runtime`: numeric GStreamer version; the report's application build
  continues to identify Orange source.

Do not export SDP, raw candidates, ports, addresses, credentials, adapter names,
account identities or arbitrary error text. Retain the existing opaque session
correlation for paired diagnostics. Extend the upload allowlist explicitly.

## Recovery contract

Initial ICE failure is eligible only before a peer ever connected. One retry
per user-initiated watch request. Cancellation, sign-out, changing friend/room,
host departure, negotiation/media errors and closing playback must not create
an automatic rejoin. The old child/transport must finish cleanup before a new
one starts. Preserve other host viewers. Keep retry state out of preferences.
The new attempt uses a new connection, not GStreamer's incomplete in-place ICE
restart implementation. Capture its outcome in normal diagnostic logs.

Firewall inspection must distinguish a definite matching application block
from default inbound policy, disabled firewall, unknown access and managed
policy. Only report what was measured. A repair must be scoped to the installed
media executable and local policy, never disable/reset a whole firewall or
alter another application/managed rule. Windows permission denial/cancellation
gets a friendly message and a reportable bounded reason.

## Validation

Use meaningful regression tests for candidate redaction and route correlation,
schema versions/invalid repair flags, current-vs-historical outcomes, initial
failure retry and exhausted budget, user cancellation/signout/close cleanup,
firewall policy classification and repair scope. Native read-only inspection
and isolated gathering are safe smoke tests; do not change this machine's real
firewall as a side effect of running automated tests. Run full Rust gates and
native Settings/retry smoke checks before completion.
