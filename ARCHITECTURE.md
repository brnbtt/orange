# Architecture

This newcomer map names the current Windows process boundaries, source
ownership, media defaults, compatibility contracts, and validation paths.

## Runtime And Processes

```text
                         HTTPS auth/update
                  +-----------------------------+
                  |                             v
+-----------------+--+       child stdout   +----------------+
| orange-tray.exe    |<----------------------| orange.exe     |
| GPUI + Win32 tray  |                       | list/login     |
|                    |-- supervises -------->| host/watch     |
| UI state + prefs   |                       +-------+--------+
+---------+----------+                               |
          | update handoff                            | WebSocket signalling
          v                                           v
+--------------------+                       +------------------+
| orange-updater.exe |                       | orange-relay     |
| waits for tray,    |                       | HTTP + /ws       |
| runs installer,    |                       | rooms/auth in RAM|
| starts successor   |                       +------------------+
+--------------------+

 host orange.exe  ===== WebRTC media, peer to peer =====> watch orange.exe
                  (relay forwards SDP/ICE, never media)
```

- `orange-tray.exe` is the long-lived desktop coordinator. It invokes list and
  login commands plus separate `orange.exe host` and `orange.exe watch` children.
- A host child owns capture, one encoder, and one WebRTC branch per viewer.
- Each watch child owns one native playback window and receive pipeline.
- The updater is a temporary successor that waits, installs, and reopens the tray.
- `orange-relay` is the production relay entry point; `orange serve` exposes the
  same signal server for local use.
- Video and audio travel directly between peers unless a separately configured
  TURN service is used. No TURN service is bundled.

## Crates

| Crate | Artifact | Responsibility |
| --- | --- | --- |
| `orange` | `orange.exe` | CLI, Windows capture, hardware encoding, WebRTC peers, playback window, overlay, local media diagnostics |
| `orange-tray` | `orange-tray.exe` | GPUI desktop UI, Win32 tray icon, child supervision, preferences, update client |
| `orange-signal` | library | Signal wire format, client, room relay, Discord identity, HTTP/WebSocket server, diagnostic upload |
| `orange-relay` | `orange-relay` | Small production process that binds `PORT` and runs `orange_signal::serve` |
| `orange-updater` | `orange-updater.exe` | Verifies handoff, waits for the tray, runs Inno Setup silently, reopens the tray |

## Tray Source Map

| File | Authoritative responsibility |
| --- | --- |
| `crates/orange-tray/src/main.rs` | Application state machine, GPUI startup, polling, child lifecycle, picker actions, update handoff, top-level tray ownership |
| `crates/orange-tray/src/view.rs` | All screen rendering and UI event wiring: signed out, home, picker, streaming, watching, settings, update banner |
| `crates/orange-tray/src/ui.rs` | Visual tokens and reusable GPUI controls, logo, buttons, cards, pills, titlebar controls, animations |
| `crates/orange-tray/src/background.rs` | Cancelled-and-joined thumbnail and avatar jobs, bounded avatar download and decode |
| `crates/orange-tray/src/capture.rs` | `PrintWindow` window stills, primary-screen stills, BGRA buffers, GPUI image conversion |
| `crates/orange-tray/src/supervisor.rs` | Finds GStreamer, launches `orange.exe`, parses child stdout/stderr, quality tiers, diagnostic retention |
| `crates/orange-tray/src/session.rs` | Reads CLI session JSON; atomically reads/writes tray preferences |
| `crates/orange-tray/src/tray.rs` | Native notification icon, message-only HWND/thread, events, bounded cleanup, fail-fast ownership policy |
| `crates/orange-tray/src/update.rs` | Beta manifest validation, pinned-host download, SHA-256, update job state, updater CLI handoff |

The tray quality table in `crates/orange-tray/src/supervisor.rs` is H.265 at 4
Mbps for 720p, 8 Mbps for 1080p, and 18 Mbps for 1440p. The CLI defaults to 25 Mbps.

## Media CLI Source Map

| File | Authoritative responsibility |
| --- | --- |
| `crates/orange/src/main.rs` | CLI schema and dispatch for list, record, loopback, serve, login/logout, host/watch, preview; timed pipeline shutdown |
| `crates/orange/src/pipeline.rs` | Codec mapping, capture/audio chains, encoder settings, record pipeline; default H.265 selection |
| `crates/orange/src/peer.rs` | Peer facade and shared signalling/WebRTC helpers, STUN setting, bus and connection-state reporting |
| `crates/orange/src/peer/host.rs` | Host session, shared capture/audio tees, viewer map, idle redraw, keyframe cadence, signal loop, final teardown |
| `crates/orange/src/peer/host_branch.rs` | Per-viewer WebRTC branches, request-pad ownership, offer creation, startup keyframes, blocked branch removal worker |
| `crates/orange/src/peer/watch.rs` | Viewer join, offer/answer handling, dynamic receive pads, playback ownership, receive teardown |
| `crates/orange/src/webrtc.rs` | WebRTC facade, loopback graph, payloaders, output ownership split, accepted-pad dispatch |
| `crates/orange/src/webrtc/receive.rs` | Transactional dynamic video/audio receive branches, decoder/sink construction, rollback |
| `crates/orange/src/webrtc/workers.rs` | First audio/video pad claims, audio-control and bitrate workers, cancellation, probe removal, joining |
| `crates/orange/src/webrtc/transport.rs` | RTP payload/caps constants and live jitterbuffer policy |
| `crates/orange/src/window.rs` | Playback owner versus passive handle, dedicated window thread, DPI/refresh helpers, bounded shutdown policy |
| `crates/orange/src/window/native.rs` | Win32 class/window/message loop, input, sizing/fullscreen, HWND context installation and destruction |
| `crates/orange/src/overlay.rs` | Shared overlay state, visibility, hit testing, volume/fullscreen/close state, scale and cache identity |
| `crates/orange/src/overlay/raster.rs` | Tiny-skia layout and rasterization of overlay clusters and icons |
| `crates/orange/src/overlay/gst.rs` | `overlaycomposition` callbacks, caps-to-overlay state, source aspect notification, composition draw callback |
| `crates/orange/src/media_diagnostics.rs` | Facade for diagnostic writer, operation timing, progress probes, WebRTC monitor |
| `crates/orange/src/media_diagnostics/writer.rs` | Bounded JSONL queue/file writer, metadata, size cap, process-global sink, joined shutdown |
| `crates/orange/src/media_diagnostics/operation.rs` | Started/finished timing records around media graph operations |
| `crates/orange/src/media_diagnostics/progress.rs` | RTP/depay/parsed/decoded pad counters, keyframes, queue overruns |
| `crates/orange/src/media_diagnostics/webrtc_monitor.rs` | Periodic sanitized WebRTC statistics and UI/media progress worker |
| `crates/orange/src/auth.rs` | Desktop OAuth polling and atomic `%APPDATA%\orange\session.json` ownership |
| `crates/orange/src/targets.rs` | Capturable-window enumeration/filtering, HWND-to-PID lookup, late-join redraw request |
| `crates/orange/src/text.rs` | System font loading and glyph rasterization for the video overlay |

## Signal And Entry Source Map

| File | Authoritative responsibility |
| --- | --- |
| `crates/orange-signal/src/lib.rs` | Media-free facade and public signal/client/relay/server exports |
| `crates/orange-signal/src/protocol.rs` | Serde-tagged `Signal` protocol and relay-controlled peer routing IDs |
| `crates/orange-signal/src/client.rs` | WebSocket client tasks, heartbeat, channels, close frame, bounded abort-and-join cleanup |
| `crates/orange-signal/src/relay.rs` | Room codes, in-memory rooms, role rules, routing, viewer cap, queues and rate limit |
| `crates/orange-signal/src/server.rs` | Axum routes, 512-connection semaphore, OAuth HTTP endpoints, `/ws`, diagnostics endpoint |
| `crates/orange-signal/src/auth.rs` | Discord OAuth exchange, pending attempts, opaque in-memory sessions, expiration and capacities |
| `crates/orange-signal/src/diagnostics.rs` | Authenticated ZIP upload, strict metadata, local/Azure Blob storage, pseudonymous keys |
| `crates/orange-relay/src/main.rs` | Production relay entry, `PORT`, Ctrl-C shutdown selection |
| `crates/orange-updater/src/main.rs` | Updater argument parser, parent wait, checksum, silent installer, restart/failure record |

## Host Media Flow

The validated production default is H.265. Frames remain D3D11-backed through
capture, GPU conversion, and hardware encoding:

```text
d3d11screencapturesrc
  -> video/x-raw(memory:D3D11Memory),framerate=...
  -> leaky queue -> d3d11convert -> optional D3D11 scale caps
  -> mfh265enc -> h265parse -> tee
  -> per-viewer leaky queue -> rtph265pay -> RTP caps -> webrtcbin
```

- The shared tee is after the parser: capture and hardware encode happen once.
- Every viewer receives its own payloader, RTP stream, WebRTC peer, offer, ICE,
  startup keyframe worker, diagnostics handle, and requested pads.
- NACK is enabled for video. Periodic keyframes and redraw requests support
  recovery and late joins, including windows that are not repainting.
- AV1/NVIDIA and H.264 are explicit diagnostic/development choices, not defaults.

Host audio is optional and does not block video:

```text
wasapi2src loopback=true [process-tree scope]
  -> leaky queue -> audioconvert -> audioresample -> stereo 48 kHz
  -> opusenc 128 kbps -> rtpopuspay -> caps -> tee -> per-viewer webrtcbin
```

Window capture supplies its process PID and excludes other applications.
Whole-screen capture uses PID zero and includes system output audio.

## Watch Media Flow

```text
webrtcbin RTP pad
  -> rtph265depay -> h265parse -> d3d11h265dec
  -> one-buffer leaky queue -> overlaycomposition -> d3d11videosink -> owned HWND

webrtcbin OPUS pad
  -> rtpopusdepay -> opusdec -> audioconvert -> audioresample
  -> volume -> wasapi2sink (fallback: wasapisink)
```

- The receive registry accepts only the first supported video pad and first
  Opus pad. A video pad consumes the one output target.
- Dynamic receive construction blocks the incoming pad, adds and links all
  elements, synchronizes them, then removes the block probe. Failures unlink,
  set added elements to Null, remove them, and remove the probe.
- Live receive latency is 100 ms. Video/RTX jitterbuffers drop at the live edge;
  Opus does not silently drop late packets and uses decoder packet-loss concealment.

## Ownership And Teardown

- Unique owners initiate teardown: `PlaybackWindow`, `Tray`, `Supervisor`,
  `SignalClient`, `ViewerBranch`, `ReceiveWorkerRegistry`, and job handles.
- Passive `PlaybackWindowHandle` clones may inspect the HWND/alive state and
  overlay, but dropping a handle never destroys the window.
- Playback owners are declared before pipelines. Receive pipelines reach
  `gst::State::Null` before the owner destroys its HWND.
- Tray and playback HWNDs are created, messaged, and destroyed on their native
  creator threads. Incomplete native cleanup is fail-fast; detaching a thread
  with unprovable HWND/context ownership is not allowed.
- Background, diagnostics, socket, keyframe, teardown, audio-control, and
  bitrate workers are cancelled or disconnected and then joined/awaited.
- Requested `webrtcbin` and tee pads are owned by branch guards and released.
  Temporary block/idle and bitrate probes are removed on rollback or teardown.
- Teardown combines primary and cleanup errors rather than hiding either one.

## Compatibility Contracts

| Contract | Authority |
| --- | --- |
| Signal JSON tags, fields, defaults, and peer stamping | `crates/orange-signal/src/protocol.rs` |
| Tray window discovery JSON from `orange list --json` | producer: `crates/orange/src/main.rs`; consumer: `crates/orange-tray/src/supervisor.rs` |
| Host stdout markers `Share this code:`, `[host] signed in as`, `[host-status] <json>` | producer: `crates/orange/src/peer/host.rs`; consumer: `crates/orange-tray/src/supervisor.rs` |
| Session file `%APPDATA%\orange\session.json` | writer: `crates/orange/src/auth.rs`; reader: `crates/orange-tray/src/session.rs` |
| Preferences `%APPDATA%\orange\preferences.json` | `crates/orange-tray/src/session.rs` |
| Beta manifest schema/host/name/hash | producer: `publish-beta.ps1`; consumer: `crates/orange-tray/src/update.rs` |
| Updater flags `--installer --sha256 --parent --install-dir` | producer: `crates/orange-tray/src/update.rs`; consumer: `crates/orange-updater/src/main.rs` |
| RTP video payload 96, RTX 97, Opus 111, clocks and 100 ms receive latency | `crates/orange/src/webrtc/transport.rs` |

Treat changes as migrations and update both sides of each contract.

## Where Do I Change...?

| Change | Start here |
| --- | --- |
| CLI flags/default codec/default CLI bitrate | `crates/orange/src/main.rs` |
| Capture elements, encoder choice, Opus send chain | `crates/orange/src/pipeline.rs` |
| Host fan-out, late join, keyframe behavior | `crates/orange/src/peer/host.rs`, `crates/orange/src/peer/host_branch.rs` |
| Viewer negotiation and session lifetime | `crates/orange/src/peer/watch.rs` |
| Decoder, depayloader, sinks, receive rollback | `crates/orange/src/webrtc/receive.rs` |
| RTP payloads, jitterbuffer latency/drop policy | `crates/orange/src/webrtc/transport.rs` |
| Native viewer behavior and HWND lifetime | `crates/orange/src/window.rs`, `crates/orange/src/window/native.rs` |
| Overlay behavior/layout | `crates/orange/src/overlay.rs`, `crates/orange/src/overlay/raster.rs`, `crates/orange/src/overlay/gst.rs` |
| Tray screens | `crates/orange-tray/src/view.rs` |
| Tray colors/components | `crates/orange-tray/src/ui.rs` |
| Tray quality tiers and child log parsing | `crates/orange-tray/src/supervisor.rs` |
| Session/preferences persistence | `crates/orange/src/auth.rs`, `crates/orange-tray/src/session.rs` |
| Signal wire format | `crates/orange-signal/src/protocol.rs` |
| Relay room policy and limits | `crates/orange-signal/src/relay.rs`, `crates/orange-signal/src/server.rs` |
| Discord OAuth/session policy | `crates/orange-signal/src/auth.rs` |
| Update manifest/client handoff | `publish-beta.ps1`, `crates/orange-tray/src/update.rs`, `crates/orange-updater/src/main.rs` |
| Installer contents/prerequisites | `package.ps1`, `packaging/windows/orange.iss` |
| Azure single-replica deployment | `deploy/azure.ps1` |

## Validation

Run from a PowerShell prompt at the repository root:

```powershell
. .\dev.ps1

# Target the package changed during development.
cargo test --locked -p orange

# Full local source gates.
cargo test --locked --workspace
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
cargo build --locked --release --workspace

# Builds and validates the per-user installer; requires Inno Setup 6.
.\package.ps1
```

Direct PowerShell contract tests do not publish or deploy:

```powershell
.\deploy\test-azure-script.ps1
.\packaging\windows\test-installer.ps1
.\packaging\windows\test-package-provenance.ps1
.\packaging\windows\test-beta-publish.ps1
.\alpha\test-orange-alpha.ps1
.\alpha\test-publish-alpha.ps1
```

External gates are installed interactive media acceptance on required GPU
vendors, two-machine WAN/TURN paths, deployment, publication, and signing.
`deploy/azure.ps1` uses a cloud source build, so local Docker availability is
not a source-level result.

## Deployment Model And Boundaries

- Azure is deliberately pinned to one always-on replica in `deploy/azure.ps1`.
- Rooms, pending OAuth attempts, and authenticated relay sessions are in memory. A revision switch, deploy, or restart interrupts every active room and signs users out. There is no horizontal-scaling claim.
- The process accepts at most 512 concurrent WebSocket connections and each room accepts at most 16 viewers.
- Each peer has a 64-message outbound queue. Inbound signalling permits 256 text messages per 10-second fixed window; WebSocket messages cap at 64 KiB.
- Auth memory is bounded to 1,024 pending attempts and 4,096 sessions, with 10-minute pending and 30-day session lifetimes while the process survives.
- Public Google STUN is configured. No TURN server is bundled, so restrictive NAT/firewall combinations remain an external connectivity boundary.
- HTTPS host restrictions and SHA-256 protect update integrity, but production promotion is blocked until a trusted external code-signing certificate is available.
