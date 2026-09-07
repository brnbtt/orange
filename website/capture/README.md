# Capture the native interface

Local capture tooling, **not website content to deploy**. It patches only a
separate disposable linked worktree; the website checkout's production crates
and all renderer files remain untouched.

Requirements: Windows desktop session, Orange's Rust/GStreamer development
environment, Python 3 with Pillow, and the current source. The fixture tracks
the current client state fields and uses the production renderers unchanged.
To reproduce the historical 1.0.1 captures, use both the scripts and source
from tag `v1.0.1`.

1. Fetch `origin/main` and create a separate worktree following `AGENTS.md`.
    Use the same revision as these capture scripts.
2. In that clean worktree, run `. .\dev.ps1` followed by
   `cargo test --locked -p orange-client`.
3. From this capture-tool directory, run:

   ```powershell
   python prepare.py C:\path\to\disposable-worktree
   ```

   This copies `fixture.rs` into temporary client source, redirects its entry
   point, seeds `code()` / `viewers()`, and invokes `game_art.py` to create
   original demo-game frames in `capture-art/`. Run once against clean source.
   The preparation script rejects the website checkout and main checkouts.

4. Build from the disposable worktree:

   ```powershell
   . .\dev.ps1
   $env:ORANGE_UPDATE_CHANNEL = 'capture'
   cargo build --locked -p orange-client
   ```

5. With Windows display scaling at 150%, run from this tool directory:

   ```powershell
   python capture.py C:\path\to\disposable-worktree
   ```

The script launches five isolated native fixture processes: Home, picker,
Streaming, profile confirmation, and Requests. It waits for paint, captures
the client area with Win32 `PrintWindow`, and terminates only those processes.
Output defaults to `../screenshots/`; use `--output PATH` for another directory.

The pointer hovers Vector Arena in the picker to match the subsequent Streaming
screen. Requests produces two direct captures: the initial inbox in
`requests-incoming.png`, then `requests.png` after a real mouse-wheel input.
Both cards now fit, so the wheel does not move this fixture. The pointer is restored.
No stitching or resizing occurs. Animation timing can cause small differences
between runs.

`fixture.rs` seeds the real mutual-friend coordinator through its existing
public `snapshot` and `synced` fields; no helper or modification to `friends.rs`
is needed. The request is offered to aimassist before becoming outgoing Pending;
respawned is incoming; fragbyte and nightshift are already mutual friends.

The fixture bypasses normal startup/polling, isolates account directories, uses
empty session tokens (a fixed fake token for Settings), and configures an unused loopback signalling address.
There is no active capture, media child or network session. `game_art.py` draws
only fictional game-preview pixels, never the Orange interface.

`ORANGE_CAPTURE_SCREEN=settings` opens the real Settings renderer with System
expanded. This fixture polls only its troubleshooting job, so Troubleshoot,
Cancel, Run again and Copy report can be exercised without account polling or
starting a stream. The diagnostic child uses the ordinary runtime lookup; its
signalling check deliberately fails against the fixture's unused loopback port.
Set `ORANGE_CAPTURE_REPORT_SERVER` to a disposable loopback HTTP test server's
`ws://.../ws` URL to exercise Send report with the fake `capture-support-token`.
No real account credentials are loaded by this fixture.

Recorded verification: polished client **112 passed, 2 ignored**;
fixture build succeeded; renderers match the source worktree verbatim;
all six final 720 × 990 PNGs visually inspected.

## Window chrome regression

Prepare and build a disposable fixture from the current source for this check,
rather than the `v1.0.1` screenshot revision (which intentionally fails). Run:

```powershell
python test-chrome.py C:\path\to\disposable-worktree
```

Use `--binary PATH` if its executable is in a shared build cache. This check
moves only the fixture process's window, restores the cursor, and terminates
the fixture afterward. It verifies that dragging the brand and empty titlebar
actually moves the HWND, while settings and body drags do not. In 1.0.1 both
titlebar regions still returned `HTCAPTION`, but GPUI's root focus handler
prevented the native mouse-down default; hit-test assertions alone missed it.

## Friend controls regression

Against a current prepared fixture, run `test-friend-controls.py` with the same
worktree and optional `--binary` arguments. It checks mouse/Space/Enter collapse
and persisted per-account mute changes. GPUI already emits keyboard clicks;
adding explicit Enter/Space handlers made the disclosure toggle twice.
Run native input checks separately from the workspace tests, which also open
Windows and can take focus.
