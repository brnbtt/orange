# Native Orange screenshots

Captured from the actual **Orange 0.9.2 GPUI Windows client**, based on revision
`5a767a02f436a82eeb21fe8624399aee40e0c0da`, on 2026-09-05.

| Asset | Native pixels | Staged state |
| --- | --- | --- |
| `home.png` | 720 × 990 | Alex signed in; Maya streaming; Jules offline |
| `pick.png` | 720 × 990 | Four fictional windows and an original staged display; Coastline hovered; 1080p selected |
| `streaming.png` | 720 × 990 | Coastline preview; 1080p / 60 fps; `ORA-NGE`; Maya and Sam watching |

The 480 × 660 logical-pixel app was rendered by GPUI at Windows 150% DPI and
captured with `PrintWindow(PW_CLIENTONLY | PW_RENDERFULLCONTENT)`. These PNGs
are direct client-area captures, without resizing, compositing or retouching.
The normal `view.rs`, `view/` and `ui/` renderer code was used unchanged.

Names, IDs, presence, source titles/dimensions, share code, viewers and preview
pixels are mocked. The Coastline landscape, idea board, prototype editor,
music player and staged display are original generated thumbnail artwork.
No real desktop, account, friends, OAuth session or production network was used.
The Streaming screen illustrates staged session state, not a live media test.

Generation used a disposable `orange-capture` worktree. Its entry point
constructs the existing `Orange` state directly, without the normal polling,
window enumeration, tray, login or process-supervision startup. The capture
build uses `ORANGE_UPDATE_CHANNEL=capture` to disable update requests.

Reproduction files and instructions are in [`../capture/`](../capture/README.md).
They are local capture tooling, outside the website's upload manifest and the
production crates. The three final PNGs were opened and visually inspected.
