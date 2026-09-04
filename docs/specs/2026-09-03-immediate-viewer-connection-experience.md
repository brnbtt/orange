# Immediate Viewer Connection Experience

## Goal

Reveal the existing Orange playback window as soon as a viewer starts joining, then show truthful connection progress in that HWND until the first decoded video frame is ready.

## User-facing states

1. **Joining room** — signaling is opening and the viewer is asking to join.
2. **Exchanging stream details** — StreamInfo and SDP offer/answer work is in progress.
3. **Finding a direct route** — ICE is gathering candidates or checking routes.
4. **Securing connection** — ICE is connected while WebRTC/DTLS is still connecting.
5. **Starting video and audio** — the peer connection is connected and media startup remains.
6. **Connected** — the first decoded video frame is ready; normal playback replaces the connection surface.

The progress surface uses `Connecting...` as its primary text. Failures replace it with concise, actionable, privacy-safe copy.

## Constraints

- Reveal the viewer promptly, ideally within 300 ms of beginning a valid join attempt.
- Do not wait for ICE, DTLS, `pad-added`, receive-branch construction, or media.
- Paint through the existing native playback HWND; do not create another window or depend on a GStreamer video buffer.
- Preserve Orange's dark surface, Segoe UI typography, spacing, and restrained orange accent.
- Keep the HWND responsive and closable during every stage.
- Closing must cancel watch work and preserve the existing ordered teardown of receive workers, pipeline, signaling tasks, and HWND ownership.
- Attach the D3D11 sink to the same HWND and let the first video frame replace the native surface without recreation, stale text, or a light flash.
- Do not add delays or change ICE priority, STUN/TURN, signaling, codecs, or media behavior.
- Never display or diagnose IP addresses, ICE candidate values, room codes, SDP, or session secrets.
- Marshal all HWND work to the creating window thread.
- Emit elapsed and per-stage timing diagnostics for each visible transition.

## Verification

- Unit-test event-to-stage mapping, monotonicity, and terminal failure behavior.
- Test early reveal without a D3D11 sink and same-HWND sink attachment.
- Test native window closure and bounded owner teardown at every pre-video stage.
- Run targeted Orange tests, preview/window lifecycle tests, formatting, linting, and all workspace tests.
- Manually compare a fast peer (~500 ms) and the known slow peer (~2.2 seconds in ICE checking) when both peers are available.
