# Reproduce the native 1.0.1 interface captures

Local capture tooling, **not website content to deploy**. It patches only a
separate disposable linked worktree; the website checkout's production crates
and all renderer files remain untouched.

Requirements: Windows desktop session, Orange's Rust/GStreamer development
environment, Python 3 with Pillow, and the source from tag `v1.0.1`.
This includes the compact home layout,
contextual friend actions, icon-labelled buttons and soft ambient lighting.
The original captures preceded the version bump and use the same renderer.

1. Fetch `origin/main` and create a separate worktree following `AGENTS.md`.
    Pin the disposable worktree to `v1.0.1` for this interface.
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
empty session tokens, and configures an unused loopback signalling address.
There is no active capture, media child or network session. `game_art.py` draws
only fictional game-preview pixels, never the Orange interface.

Recorded verification: polished client **112 passed, 2 ignored**;
fixture build succeeded; renderers match the source worktree verbatim;
all six final 720 × 990 PNGs visually inspected.
