# Working On Orange

Notes for agents. `ARCHITECTURE.md` says where the code lives and `README.md`
says what it does; neither covers the parts of this machine and this workflow
that have already cost sessions real time. That is what this file is for.

Read `ARCHITECTURE.md` first. Come back here before you branch, publish, or
read a diagnostic log.

## Orient Before You Touch Anything

`C:\Users\brnbt\DEV\orange` is the main checkout, and **it is regularly stale**.
Several agents work in parallel worktrees and land on `main` independently, so
the tree you open may be several releases behind by the time you read it. One
session wrote a complete fix against a checkout three releases old, and the
diagnosis behind it was drawn from source that production had not run for two
days.

Start every session with:

```powershell
$git = Get-ChildItem "$env:LOCALAPPDATA\Temp\opencode" -Filter git.exe -Recurse -Depth 4 |
       Select-Object -First 1 -ExpandProperty FullName
& $git -C C:\Users\brnbt\DEV\orange fetch origin main
& $git -C C:\Users\brnbt\DEV\orange rev-list --count HEAD..origin/main   # 0, or you are behind
```

## This Machine

| Tool | Where | Notes |
| --- | --- | --- |
| `git` | portable MinGit under `%LOCALAPPDATA%\Temp\opencode\mingit-*\git\cmd\git.exe` | **Not on `PATH`.** `Get-Command git` fails. Discover it with the snippet above rather than hardcoding a version. |
| Inno Setup 6 | `%LOCALAPPDATA%\Programs\Inno Setup 6\ISCC.exe` | Not on `PATH` and not under Program Files. `package.ps1` finds it itself, so never gate on `Get-Command iscc`. |
| GStreamer | `%LOCALAPPDATA%\Programs\gstreamer\1.0\msvc_x86_64` | Dot-source `dev.ps1` first or every `cargo` command fails in `gobject-sys`. |
| `az`, `gh` | on `PATH` | Both already authenticated. |

`. .\dev.ps1` is required before any `cargo` or `gst-inspect-1.0` invocation. It
is idempotent and cheap; just run it.

For the native UI capture scripts, bare `python` currently resolves to the
Microsoft Store alias. The interpreter at
`%LOCALAPPDATA%\Temp\opencode\vtracer-env\Scripts\python.exe` has Pillow and
runs `website/capture/prepare.py`; the nearby `cd-venv` interpreter lacks Pillow.

Do not run `git add -A` in the root checkout. `.agents/` and `skills-lock.json`
are untracked and **not** ignored, so a blanket add sweeps the whole local skill
library into a commit. Stage paths explicitly there, or work in a worktree,
where those files do not exist.

## Worktrees

Worktrees live in `%LOCALAPPDATA%\Temp\opencode\<name>`, not in `.worktrees/`.
`.cargo/config.toml` currently selects `rust-lld`, not a shared `target-dir`.
A fresh worktree builds dependencies into its own `target/`; the A/V fix's
first workspace test run spent about 90 seconds compiling before tests began.

**Always branch from `origin/main` after fetching**, never from whatever the
current checkout happens to be at:

```powershell
& $git -C C:\Users\brnbt\DEV\orange worktree add `
    "$env:LOCALAPPDATA\Temp\opencode\orange-<topic>" -b <type>/<topic> origin/main
```

Then verify a clean baseline before editing, so a later failure is unambiguously
yours:

```powershell
. .\dev.ps1
cargo test --locked --workspace
```

## Releasing

`ship.ps1` bumps the version, commits, pushes `origin main`, builds the
installer, uploads it to Azure and cuts the GitHub release. Read its header
comment; it is accurate.

Five things that are not obvious:

1. **It pushes `origin main`, so `main` must be checked out and must already
   contain your work.** From a topic-branch worktree it will push the *local*
   `main` ref, not your branch. Fast-forward `main` onto your branch in the root
   checkout and ship from there.
2. **It refuses a dirty tracked tree.** It only commits the version bump.
   Untracked files are fine.
3. **`ConfirmImpact` is `High`.** Non-interactive runs need `-Confirm:$false` or
   they hang on a prompt.
4. **It calls a bare `git`, which is not on `PATH` here.** Without MinGit
   prepended it dies immediately with `CommandNotFoundException` at
   "Checking the working tree". Prepend it first:
   `$env:PATH = "$(Split-Path $git);$env:PATH"`.
5. **Never pipe it through `2>&1 |`.** It sets `ErrorActionPreference = "Stop"`,
   and merging a native command's stderr into the pipeline turns cargo's
   ordinary `Compiling ...` progress into a terminating error. It will die
   mid-run for no real reason. Let it write to the console.

   **`deploy/azure.ps1` has the same defect**, and it is easier to trip because
   the trigger is instant: `az containerapp up` writes `WARNING: The behavior of
   this command has been altered by the following extension: containerapp` to
   stderr on every single run. Piped, the deploy dies on that line before it
   builds anything.
   A root-checkout deploy also stalled before any ACR run was created; its
   source tree contained 15 old installers under `dist/` and a full build cache.
   `.dockerignore` now excludes release artifacts and the local agent library,
   using bare directory names for Azure archiver compatibility. A clean
   deployment worktree avoids uploading local artifacts altogether.
6. **If it fails, check before assuming damage.** Everything that can fail
   cheaply runs before anything mutates the repo, so an early failure leaves the
   version, the commits and the remote untouched. Verify with `git log` and
   `Cargo.toml` rather than guessing.

7. **`ship.ps1 -SkipTests` currently still runs the tests.** Dot-sourcing
   `publish-beta.ps1 -LibraryOnly` resets the shared `$SkipTests` parameter to
   false. This repeated the parallel suite after a passing serial release gate
   and hit the subprocess deadlines again. Set `$env:RUST_TEST_THREADS = "1"`
   for the shipping process when using the serial workaround below; it also
   applies to that extra test run. Do not assume the switch skipped it.

If the *publish* fails partway, do **not** re-run `ship.ps1` — it would bump the
version a second time. Re-run the idempotent publisher instead:

```powershell
.\publish-beta.ps1 -Publish -Notes "<same notes>"
```

Release notes are user-facing, capped at 500 bytes, and must not contain control
characters. Confirm what actually reached users:

```powershell
Invoke-WebRequest -UseBasicParsing `
  "https://orangealpha0d8d5893e69a3.blob.core.windows.net/releases/orange-beta.json"
```

A docs-only or comment-only change does not need a release. Commit and push
`main` directly; do not bump a version for it.

## Reading Diagnostic Logs

Sessions write JSONL to `%LOCALAPPDATA%\orange\diagnostics\orange-media-<pid>.jsonl`.
`ORANGE_MEDIA_DIAGNOSTICS` is set by the client when it spawns children, not in
the user environment, so it looks unset from a normal shell.

**A running session's log looks empty.** NTFS does not update the directory
entry while a process holds the handle open, so `Get-ChildItem` reports 0 bytes
for a file that already has megabytes in it. The writer flushes every line
(`media_diagnostics/writer.rs`), so data is never actually pending. Read the
file, or wait for the process to exit, before concluding anything is missing.

**Every line carries a `build` field with the exact commit.** Check it and read
that revision's source, not your working tree:

```powershell
& $git -C C:\Users\brnbt\DEV\orange show <build>:crates/orange/src/webrtc/receive.rs
```

Skipping this produced a confident, wrong diagnosis of an audio fault: the
analysed source had a defect that the build in the log had already fixed.

Useful fields when triaging media: `media-progress` carries per-stage buffer,
byte, `pts_ms` and `av_offset_ms` values; `webrtc-stats` carries loss, jitter,
NACK/PLI/FIR and jitterbuffer counters. `av_offset_ms` compares sink-input
timestamps, not physical playback. The native regression uses output capture
because these probes run before the audio ringbuffer and video presentation.
Use an output-captured marker comparison to validate playout (see the hardware
acceptance test in `webrtc/receive_playout_tests.rs`).

## Diagnosis Discipline

The media stack punishes plausible reasoning. Two failure modes to avoid, both
observed:

- **Do not present inference as measurement.** A number derived from adding up
  the latency budget is a hypothesis. Say which one you have. If the log does
  not contain the number you need, add it and ship the instrumentation before
  claiming a cause.
- **Zero packet loss does not mean the transport is healthy.** A host asking for
  180 fps delivered 53.6 with nothing lost in transit — the encoder simply never
  produced the frames. Compare what was requested against what arrived, not just
  the error counters.

`.agents/skills/orange-media-debugging` has the layer-by-layer ladder. Use it
rather than reasoning from the source alone.

## Tool Discipline & Execution Speed

- **Never use `shell` for filesystem inspection.** Spawning PowerShell incurs ~150-250ms process overhead per call. Always use built-in tools (`read`, `grep`, `glob`, `edit`, `write`), which run in-process with 0ms process startup latency.
- **Parallelize independent operations in a single turn.** Call multiple independent reads, globs, or searches concurrently rather than issuing them sequentially.
- **Use parallel subagents for multi-area research.** When investigating multiple independent components, spawn background subagents (`agent: "explore"` or `"general"` with `background: true`) to execute in parallel child sessions.
- Reserve `shell` strictly for native builds, cargo, and git commands.

## Flaky Under Load

`peer_worker_review_*` in `crates/orange` spawn child processes and kill them
on a deadline (`crates/orange/src/test_support.rs`). Run immediately after a
release build or another full suite they can miss that deadline and fail, which
looks alarming when the change under test was in another crate entirely. Re-run
the named tests on an idle machine before believing it. Two failed this way
during a change that touched only `orange-signal` and `orange-client`.

## Bulk Renames

PowerShell here is **5.1**, where `` `u{XXXX} `` is not an escape. A rename that
protected Win32 literals with `` "`u{0001}SENTINEL`u{0001}" `` wrote the literal
text `u{0001}SENTINEL u{0001}` into the source, and the restore then failed a
second time because `{0001}` is a regex quantifier. `Shell_TrayWnd` and
`Shell_SecondaryTrayWnd` in `crates/orange/src/targets.rs` became strings
Windows has never heard of. **It compiled**, because they are string literals;
the only symptom would have been the taskbar appearing in the window picker.

Two things follow. Check `$PSVersionTable.PSVersion` before using any escape
newer than PowerShell 5.1. And after a bulk rename, grep for the term you
replaced and confirm the survivors are exactly the ones you intended — a search
returning *nothing* is a failure, not a success, when you meant to keep some.

## Conventions

- Commit subjects are imperative and descriptive, no prefixes: `Cap capture at
  120 fps`, `Prevent silent live audio startup`. Bodies explain the evidence and
  the reasoning, and say plainly what the change does *not* fix.
- Commit messages with quotes or apostrophes break PowerShell argument parsing.
  Write the message to a file and use `git commit -F <file>`.
- Comments explain why, tied to the concrete thing that went wrong. They are not
  decoration, and the codebase is consistent about this — match it.
- Full local gate before shipping: `cargo test --locked --workspace
  --all-features`, `cargo clippy --locked --workspace --all-targets
  --all-features -- -D warnings`, `cargo fmt --all -- --check`.
- The PowerShell contract tests are part of that gate for any packaging or
  deployment change: `packaging\windows\test-installer.ps1`,
  `test-package-provenance.ps1`, `test-beta-publish.ps1`,
  `deploy\test-azure-script.ps1`. `test-package-provenance.ps1` shells out to a
  bare `git`, which is not on `PATH` here, so it fails with
  `CommandNotFoundException` until you prepend MinGit:
  `$env:PATH = "$(Split-Path $git);$env:PATH"`. That failure is the environment,
  not the change.
- Run each PowerShell contract test in its own `powershell.exe -NoProfile
  -ExecutionPolicy Bypass -File <script>` process when combining checks.
  Calling `test-installer.ps1` directly stopped a four-test shell sequence at
  its `exit 0`; the other three tests never ran despite the successful exit.
- Tests are named as sentences describing the behaviour they protect, and carry
  a comment explaining the real failure that motivated them.

## Keeping This File Current

Add an entry when you lose time to something that was not written down, and
delete entries that stop being true. This file earns its place only by being
accurate — a stale warning is worse than no warning, because it gets trusted.

State what actually happened rather than a generic rule. "`ship.ps1` dies if you
pipe it through `2>&1`" is useful; "be careful with PowerShell redirection" is
not.

If a fact belongs to the code rather than the workflow, it goes in
`ARCHITECTURE.md` or a comment at the site instead. Keep this file about the
things you cannot learn by reading the source.
