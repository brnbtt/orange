# Home Polish Implementation Plan

**Goal:** Compact, aligned social controls and streaming actions, contextual friend removal, and a quieter animated backdrop.

**Architecture:** Keep the existing GPUI controls and friends mutation path. Use GPUI anchored/deferred elements for the friend menu; keep its transient state separate from persisted preferences. Refine the ambient layer with existing gradient and animation primitives.

**Tech Stack:** Rust, GPUI 0.2.2, Windows.

**Spec:** User screenshot and design feedback in this session.

## Tasks

- [x] Verify the clean workspace baseline (381 tests passed serially after documented media subprocess timeouts).
- [x] `view/home.rs`: compact header with aligned 30px actions, one short hint, selected Friends/Requests tabs with counts, compact friend rows, and a single row of 40px Start streaming / Join with a code actions. Remove the concentric start icon.
- [x] `view/home.rs`, `view.rs`, `main.rs`: right-click friend menu targeting stable friend IDs; keyboard entry through the options button, focus feedback, Escape/click-away dismissal and stale-state cleanup. Reuse `remove_friend`; update `website/capture/fixture.rs`. Bind Tab/Shift-Tab traversal explicitly because GPUI only records tab stops.
- [x] `ui/decor.rs`, `ui/theme.rs`: faint wide stationary grid, edge fade, slow warm ambient light, preserving the inactive-window animation gate.
- [x] Compile, run client tests and clippy, check formatting, and inspect native fixture screenshots and menu interactions (112 client tests passed, 2 ignored; clippy and formatting passed).

## Acceptance

At the existing 480×660 logical window size, the header actions share a baseline, the selected tab follows the displayed content, the bottom controls occupy one row, and the list has more room. Removal appears only in a contextual menu, dismissing never mutates the roster, and activation targets the clicked friend after roster reorder. The backdrop does not interfere with text or schedule animation while inactive.

## Native verification

Using the existing capture fixture in a separate disposable worktree, inspected Home, Requests, Add friend, picker and Streaming at 720×990 pixels (150% DPI). Verified right-click and options-button entry, window-edge snapping, Escape and click-away dismissal, keyboard-only entry from launch, Tab dismissal/next-friend traversal, and keyboard removal enqueueing the existing mutation (the fixture has no production account). Two timed captures confirmed the warm ambient light changes while active.

The keyboard checks caught two GPUI integration details: tab stops need an explicit Tab binding, and the frame needs focus before any child has it. Both are handled in `view.rs`.

The review follow-up also verified focus restoration after click-away, removal, and the native activation-change event. Blank-space clicks do not move focus to the frame; a menu dismissed after its target disappears instead falls back to the frame because that row no longer exists.

The larger-roster follow-up exercised 20 and 256 friends, long names, mouse scrolling through the last row, and menus at the viewport bottom. It exposed missing keyboard reveal: tabbing to friend 27 originally left the viewport at friend 1. The list now tracks its scroll handle and reveals the newly focused row on Tab key-up. Native captures verify friend 27 and its menu are visible. These are functional/debug checks, not release-performance results; the active debug process used roughly one CPU core at both roster sizes.

## Button and website follow-up

Added compact vector screen-share and enter-stream icons to the bottom buttons, with the existing labels and keyboard actions retained. Refreshed all six website captures and aligned its frames, buttons and ambient lighting with the client. Published the static website on 2026-09-06 with an explicit unreleased-interface preview label; the download still resolves to app 1.0.0.

Verification: 112 client tests passed; clippy, formatting, website release tests and deployment contract tests passed. Live browser checks covered 320/375/768/1440px widths, loaded images, horizontal overflow, keyboard skip link, FAQ, reduced motion and no-JavaScript fallback. Live CSS and all six PNGs match local bytes; both downloads match the public manifest and the custom 404 works.
