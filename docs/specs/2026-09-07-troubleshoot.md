# Settings troubleshooting

Add a Troubleshoot action beside the diagnostic tools in Settings. It runs
on demand, stays responsive, and produces an on-screen, copyable report.

The motivating report contains two viewer logs from build 20ba4ee: ICE
gathering completes, ICE Checking becomes Failed, and no media arrives. Those
observations identify a failed layer, not the firewall, NAT type, or the
correctness of candidate delivery.

## Required behavior

- Check the installed media executable/runtime, relevant capture/codec/playback
  capabilities, the configured signalling endpoint, and UDP STUN reachability.
- Report each check as passed, failed, or inconclusive with a specific next
  step. A detected factory is availability evidence, not a successful hardware
  streaming test. A STUN response is reachability evidence, not peer readiness.
- Review bounded recent diagnostic data separately from current checks. Identify
  ICE failure, post-ICE peer failure, or received media only when observed.
  Include the log build and timestamp; handle missing and incomplete logs.
- State that a real connection to the intended friend is still needed to verify
  end-to-end readiness. No TURN support is added by this feature.
- Run at most one check job; support cancellation and bounded cleanup. Network
  waits and media startup must not block the GPUI thread.
- Copy only allowlisted diagnostic facts. Exclude tokens, account identities,
  room/session IDs, raw SDP/candidates, network addresses, and arbitrary errors.
- Match existing Settings cards, typography, focus traversal, scrolling and
  button patterns. Show running feedback and readable text statuses.

## Validation

Use deterministic tests for protocol validation, timeout/failure handling,
historical diagnosis, report sanitization, and child lifecycle. Run the workspace
tests, clippy and formatting checks. Exercise the actual diagnostic command on
this machine; report external network or hardware limits honestly.
