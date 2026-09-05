# Reproduce the native screenshots

This is local capture tooling, **not website content to deploy**. It adds a
temporary Rust fixture only to a separate disposable worktree. The website
checkout's production crates are never patched.

Requirements: Windows desktop session, the Orange Rust/GStreamer development
environment, Python 3 with Pillow, and a clean worktree at
`5a767a02f436a82eeb21fe8624399aee40e0c0da` (Orange 0.9.2).

1. Fetch `origin/main` and create a separate worktree following `AGENTS.md`.
   Check the revision above if reproducing these exact screenshots.
2. In the clean worktree, run `. .\dev.ps1` then
   `cargo test --locked -p orange-client`.
3. From this directory, run:

   ```powershell
   python prepare.py C:\path\to\disposable-worktree
   ```

   This copies `fixture.rs` into the temporary client source, redirects its
   entry point, seeds `code()` / `viewers()`, and creates original artwork in
   `capture-art/`. It deliberately refuses to patch the website checkout.
   Run once against clean source.

4. Build from the disposable worktree:

   ```powershell
   . .\dev.ps1
   $env:ORANGE_UPDATE_CHANNEL = 'capture'
   cargo build --locked -p orange-client
   ```

5. With Windows display scaling set to 150%, run from this directory:

   ```powershell
   python capture.py C:\path\to\disposable-worktree
   ```

   The script launches three isolated fixture processes, waits for the native
   windows to paint, captures their client areas with Win32 `PrintWindow`, and
   terminates only those processes. It hovers Coastline in the picker to match
   the next screen, then restores the pointer. Output defaults to
   `../screenshots/`; use `--output PATH` to save elsewhere.

The fixture bypasses the normal startup/polling paths; account directories are
isolated; session tokens are empty; the configured signalling address is local
and unused. There is no active capture, media child or network session.
`view.rs`, `view/`, `ui.rs`, and `ui/` are untouched, including animations and
hover rendering. Animation timing can cause tiny pixel differences on reruns.

Recorded verification: clean client baseline **95 passed, 2 ignored**; fixture
build succeeded; all three 720 × 990 final captures visually inspected.
