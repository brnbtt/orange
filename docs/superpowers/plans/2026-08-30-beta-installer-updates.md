# Orange Beta Installer And Updates Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Deliver Orange `0.2.0-beta.1` through a one-click Windows installer with user-initiated in-app updates.

**Architecture:** The tray owns update discovery and download state while a dependency-light helper process applies the verified installer after the tray exits. A strict public Azure manifest points to versioned installers; the Inno installer remains the single source of installation and prerequisite behavior.

**Tech Stack:** Rust 2021, GPUI 0.2, Reqwest 0.12 blocking client, Semver, Windows process APIs, Inno Setup 6, PowerShell 5.1, Azure Blob Storage.

**Spec:** `docs/superpowers/specs/2026-08-30-beta-installer-updates.md`

## Global Constraints

- Version is `0.2.0-beta.1`.
- Preserve unrelated `.gitignore`, `crates/orange/src/auth.rs`, `.agents/`, and `skills-lock.json` changes in the main checkout.
- Never force an update or interrupt streaming before the user clicks `Update now`.
- Update checks and downloads are nonfatal and contain no identity or telemetry.
- Reject non-HTTPS, non-Orange-host, oversized, hash-mismatched, malformed, current, and downgrade updates.
- Keep the prior installed version usable unless Inno Setup completes successfully.
- This beta may be unsigned; explicitly document the SmartScreen limitation.
- Use test-first development for Rust and testable packaging behavior.

---

### Task 1: Update Manifest And Download Engine

**Files:**
- Create `crates/orange-tray/src/update.rs`
- Modify `crates/orange-tray/src/main.rs`
- Modify `crates/orange-tray/Cargo.toml`
- Modify `Cargo.toml`

**Interfaces:**
- `UpdateInfo` contains validated version, build, installer URL, SHA-256, and notes.
- `check_for_update(current_version) -> Result<Option<UpdateInfo>>` performs a bounded network check.
- `download_update(&UpdateInfo) -> Result<PathBuf>` streams at most 250 MiB to a temporary file and atomically retains the verified installer.
- `UpdateEvent` carries check/download results from worker threads to the tray.

- [ ] Add parser/version/URL/hash/size/cache tests and observe RED.
- [ ] Implement strict manifest parsing, semver ordering, pinned-host validation, bounded HTTP clients, streaming download, and atomic verified cache.
- [ ] Add startup and six-hour background checks without blocking GPUI.
- [ ] Run focused and workspace tests, format, and commit.

### Task 2: Detached Update Applicator

**Files:**
- Create `crates/orange-updater/Cargo.toml`
- Create `crates/orange-updater/src/main.rs`
- Modify `Cargo.toml`
- Modify `crates/orange-tray/src/update.rs`

**Interfaces:**
- `orange-updater --installer <path> --sha256 <hash> --parent <pid> --install-dir <path>`.
- The tray copies the helper to a unique temporary path and launches it with no console.

- [ ] Add argument, hash, and installer-exit policy tests and observe RED.
- [ ] Implement parent-process waiting, second hash verification, silent installer execution, success-only tray restart, bounded error logging, and temporary helper cleanup scheduling.
- [ ] Integrate one-click handoff from tray update state and commit.

### Task 3: Update Banner UX

**Files:**
- Modify `crates/orange-tray/src/main.rs`
- Test in the existing `#[cfg(test)]` module.

**Interfaces:**
- Global banner states: available, downloading, failed; checking/current remain unobtrusive.
- `Update now` begins one download; `Retry` starts a new check or download after failure.

- [ ] Add state-transition and action-label tests and observe RED.
- [ ] Render a stable-height banner using existing Orange typography, colors, and button patterns with visible click/progress/error feedback.
- [ ] On verified download, stop media children, launch the detached helper, and quit GPUI.
- [ ] Run focused and workspace tests, then commit.

### Task 4: One-Click Installer

**Files:**
- Modify `packaging/windows/orange.iss`
- Modify `package.ps1`
- Create `packaging/windows/test-installer.ps1`

**Interfaces:**
- Installer includes `orange.exe`, `orange-tray.exe`, and `orange-updater.exe`.
- Interactive install has no choice pages and launches Orange.
- Silent update skips `[Run]`; the updater reopens Orange.

- [ ] Add static installer assertions and observe RED.
- [ ] Hide all choice/completion pages, remove optional shortcut tasks, keep Start Menu/uninstall entries, and skip already-installed prerequisites.
- [ ] Embed build ID and build/package all three binaries.
- [ ] Compile installer and run static/parser checks, then commit.

### Task 5: Beta Publication

**Files:**
- Create `publish-beta.ps1`
- Create `packaging/windows/test-beta-publish.ps1`
- Modify `README.md`

**Interfaces:**
- Produces and validates `dist/orange-beta.json` and the versioned installer.
- `-Publish` uploads installer first and manifest last to the public `releases` container and creates a prerelease GitHub mirror.

- [ ] Add manifest and ordering validation tests and observe RED.
- [ ] Implement local validation and explicit publication.
- [ ] Document installation, update behavior, local cache/error paths, and unsigned-beta warning.
- [ ] Run tests and dry-run packaging, then commit.

### Task 6: End-To-End Beta Verification

**Files:**
- Modify only when a failing verification has a reproducing test.

**Interfaces:**
- Produces the first verified beta installer and public manifest.

- [ ] Run formatting, all workspace tests, release builds, PowerShell tests, and Inno compilation.
- [ ] Install over the current release and verify automatic launch.
- [ ] Serve a locally newer manifest, verify banner and one-click handoff, and confirm installer replacement/relaunch.
- [ ] Verify current/downgrade/malformed/offline manifests do not interrupt the app.
- [ ] Review the complete update trust boundary and preserve an explicit Authenticode blocker for production.
