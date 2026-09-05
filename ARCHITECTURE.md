# Architecture

This newcomer map names the current Windows process boundaries, source
ownership, media defaults, compatibility contracts, and validation paths.

## Runtime And Processes

```text
                         HTTPS auth/update
                  +-----------------------------+
                  |                             v
+-----------------+--+       child stdout   +----------------+
| orange-client.exe  |<----------------------| orange.exe     |
| GPUI + Win32 icon  |                       | list/login     |
| UI state + prefs   |-- supervises -------->| host/watch     |
| friends + presence |                       +-------+--------+
+----+----------+----+                               |
     |          | update handoff                     | WebSocket signalling
     |          v                                    v
     |  +--------------------+           +----------------------+
     |  | orange-updater.exe |           | orange-relay         |
     |  | waits for client,  |           | HTTP + /ws           |
     |  | runs installer,    |           | rooms in RAM         |
     |  | starts successor   |           | sessions in Table    |
     |  +--------------------+           +----------+-----------+
     |                                              |
     +---- GET /presence?ids= (bearer, polled) -----+

 host orange.exe  ===== WebRTC media, peer to peer =====> watch orange.exe
                  (relay forwards SDP/ICE, never media)
```

- `orange-client` is the long-lived desktop coordinator. It invokes list and
  login commands plus separate `orange.exe host` and `orange.exe watch` children.
- A host child owns capture, one encoder, and one WebRTC branch per viewer.
- Each watch child owns one native playback window and receive pipeline.
- The updater is a temporary successor that waits, installs, and reopens the client.
- `orange-relay` is the production relay entry point; `orange serve` exposes the
  same signal server for local use.
- Direct ICE media and public STUN are implemented. No TURN configuration exists, so TURN-required networks are unsupported.

## Crates

| Crate | Artifact | Responsibility |
| --- | --- | --- |
| `orange` | `orange.exe` | CLI, Windows capture, hardware encoding, WebRTC peers, playback window, overlay, local media diagnostics |
| `orange-client` | `orange-tray.exe` (see below) | GPUI desktop UI, Win32 notification icon, child supervision, preferences, friends list and presence polling, update client |
| `orange-signal` | library | Signal wire format, client, room relay, presence, Discord identity, durable sessions, HTTP/WebSocket server |
| `orange-relay` | `orange-relay` | Small production process that binds `PORT` and runs `orange_signal::serve` |
| `orange-updater` | `orange-updater.exe` | Verifies handoff, waits for the client, runs Inno Setup silently, reopens the client |

Compile dependencies point `orange -> orange-signal <- orange-relay`; client/updater communicate through processes/files and have no workspace crate dependency.

### Layout Conventions

- A module with submodules is a `foo.rs` file beside a `foo/` directory, never
  a `foo/mod.rs`. The facade file names its submodules and says what each one
  owns, so the directory listing and the file agree.
- Shared source assets live in the workspace `assets/` directory. `icon.ico` is
  compiled into both executables and is why it is not in either crate.
- Tests are inline `#[cfg(test)] mod tests` blocks, except where the block grew
  past roughly 450 lines and stopped being readable next to the code it covers.
  Those live in a `<module>_tests.rs` sibling pulled in with
  `#[cfg(test)] #[path = "..."] mod tests;`, which keeps the test paths and the
  privacy exactly as they were: `relay.rs`, `window.rs`,
  `media_diagnostics/writer.rs`, `peer/host_branch.rs` and
  `orange-client/src/update.rs`. They stay as unit-test submodules rather than
  moving to `tests/` because they cover private items that integration tests
  cannot reach; `orange` and `orange-client` are also binary-only crates.
- `docs/` holds point-in-time specs and plans. This file is the current-state
  document; nothing in `docs/` supersedes it.

The `orange-client` crate still builds an artifact named `orange-tray.exe`, set
by `[[bin]]` in its `Cargo.toml`. The updater that performs an upgrade is the
one already installed, so it only learns the new name from the release that
teaches it, and `orange.iss` has no `[InstallDelete]` — renaming the artifact
before every install carries that updater would leave both binaries present and
reopen the older one, which would find the same update waiting and loop.
`crates/orange-updater/src/main.rs` already accepts both names.

## Client Source Map

| File | Authoritative responsibility |
| --- | --- |
| `crates/orange-client/src/main.rs` | Application state machine, GPUI startup, polling, child lifecycle, picker actions, update handoff, top-level notification-icon ownership |
| `crates/orange-client/src/view.rs` | The shared frame, the entry animation, and the dispatch from `Screen` to its renderer |
| `crates/orange-client/src/view/chrome.rs` | Custom titlebar, breadcrumb, window controls |
| `crates/orange-client/src/view/toast.rs` | The floating notice layer: update banner and error toast |
| `crates/orange-client/src/view/home.rs` | Signed-out screen, home screen, friend rows |
| `crates/orange-client/src/view/pick.rs` | Share picker: quality choice, whole-display row, window cards |
| `crates/orange-client/src/view/stream.rs` | Streaming and watching screens |
| `crates/orange-client/src/view/settings.rs` | Settings sections and the cards inside them |
| `crates/orange-client/src/ui.rs` | Design-layer facade: what each `ui/` file owns and the rule that keeps them apart |
| `crates/orange-client/src/ui/theme.rs` | Design tokens: colour, type, metrics, motion. No elements |
| `crates/orange-client/src/ui/controls.rs` | Reusable GPUI controls: buttons, pills, cards, rows, titlebar |
| `crates/orange-client/src/ui/mark.rs` | The logo, its states, and the glow behind it |
| `crates/orange-client/src/ui/decor.rs` | Ambient layer: drifting grid, viewfinder brackets, registration marks |
| `crates/orange-client/src/sound.rs` | Synthesised cues for things that happen while the user is looking elsewhere |
| `crates/orange-client/src/background.rs` | Owned discovery, thumbnail and avatar jobs; nonblocking cancellation, coalesced replacement, joined cleanup, bounded avatar download/decode and failed-avatar retry backoff |
| `crates/orange-client/src/background_tests.rs` | Background-job ownership, cancellation, retry and HTTP reuse regression tests |
| `crates/orange-client/src/presence.rs` | Throttled `/presence` polling off the UI thread with a reusable HTTP client, signed-out versus unreachable, decoding friend state and profile |
| `crates/orange-client/src/capture.rs` | `PrintWindow` window stills, primary-screen stills, BGRA buffers, GPUI image conversion |
| `crates/orange-client/src/supervisor.rs` | Finds GStreamer (bundled copy first), launches `orange.exe`, bounds/cancels window enumeration and owns its output readers, parses child stdout/stderr, resolution/frame-rate choices, diagnostic retention |
| `crates/orange-client/src/supervisor_list_tests.rs` | Real-child tests of enumeration output, cancellation, deadlines and errors |
| `crates/orange-client/src/session.rs` | Reads CLI session JSON including the relay token; atomically reads/writes client preferences and the friend roster |
| `crates/orange-client/src/client.rs` | Native notification icon, message-only HWND/thread, events, bounded cleanup, fail-fast ownership policy |
| `crates/orange-client/src/update.rs` | Beta checks, fixed-host/manifest validation, SHA-256 download verification, jobs, updater handoff |
| `crates/orange-client/src/update_tests.rs` | The `update.rs` test module, in a sibling file because it outgrew the module |

The client requests automatic zero-copy encoder selection and leaves bitrate selection to the media child after output resolution and frame rate are known. The measured automatic policy anchors at 18 Mbps for 1080p60, scales sublinearly with pixels and linearly with frame rate, and caps at 80 Mbps with a client-visible quality warning. Selection prefers H.265 (Media Foundation, then NVIDIA) and falls back to H.264 (Media Foundation, then NVIDIA). The CLI defaults to explicit Media Foundation H.265; `--bitrate` remains an advanced override.

Picker enumeration and capture run off the UI thread. Navigation requests
cancellation without waiting for an in-flight native capture or avatar request;
the job owner retains and reaps the worker, discards stale results, and coalesces
replacement work instead of accumulating threads. Final shutdown still joins
owned workers. Avatar batches reuse an HTTP client and failed friend-avatar
requests wait 30 seconds before retrying (changed URLs are eligible immediately).
Decorative live dots use the same active-window
animation gate as the grid and logo aura.

## Media CLI Source Map

| File | Authoritative responsibility |
| --- | --- |
| `crates/orange/src/main.rs` | CLI schema and dispatch for list, record, loopback, serve, login/logout, host/watch, preview; timed pipeline shutdown |
| `crates/orange/src/pipeline.rs` | Codec mapping, zero-copy encoder compatibility selection, capture/audio chains, encoder settings, record pipeline |
| `crates/orange/src/peer.rs` | Peer facade and shared signalling/WebRTC helpers, STUN setting, bus and connection-state reporting |
| `crates/orange/src/peer/host.rs` | Host session, shared capture/audio tees, viewer map, idle redraw, keyframe cadence, signal loop, final teardown |
| `crates/orange/src/peer/host_branch.rs` | Per-viewer WebRTC branches, request-pad ownership, offer creation, startup keyframes, blocked branch removal worker |
| `crates/orange/src/peer/host_branch_tests.rs` | The `host_branch.rs` test module, in a sibling file because it outgrew the module |
| `crates/orange/src/peer/watch.rs` | Viewer join, offer/answer handling, dynamic receive pads, playback ownership, receive teardown |
| `crates/orange/src/webrtc.rs` | WebRTC facade, loopback graph, payloaders, output ownership split, accepted-pad dispatch |
| `crates/orange/src/webrtc/receive.rs` | Transactional dynamic video/audio receive branches, decoder/sink construction, rollback |
| `crates/orange/src/webrtc/workers.rs` | First audio/video pad claims, audio-control and bitrate workers, cancellation, probe removal, joining |
| `crates/orange/src/webrtc/transport.rs` | RTP payload/caps constants and live jitterbuffer policy |
| `crates/orange/src/window.rs` | Playback owner versus passive handle, dedicated window thread, DPI/refresh helpers, bounded shutdown policy |
| `crates/orange/src/window_tests.rs` | The `window.rs` test module, in a sibling file because it outgrew the module |
| `crates/orange/src/window/native.rs` | Win32 class/window/message loop, input, sizing/fullscreen, HWND context installation and destruction |
| `crates/orange/src/overlay.rs` | Shared overlay state, visibility, hit testing, volume/fullscreen/close state, scale and cache identity |
| `crates/orange/src/overlay/raster.rs` | Tiny-skia layout and rasterization of overlay clusters; fixed icon artwork cached independently of animation alpha, bounded to 32 rasters / 1 MiB per drawing thread |
| `crates/orange/src/overlay/gst.rs` | `overlaycomposition` callbacks, caps-to-overlay state, source aspect notification, composition draw callback |
| `crates/orange/src/media_diagnostics.rs` | Facade for diagnostic writer, operation timing, progress probes, WebRTC monitor |
| `crates/orange/src/media_diagnostics/writer.rs` | Bounded JSONL queue/file writer, metadata, size cap, process-global sink, joined shutdown |
| `crates/orange/src/media_diagnostics/writer_tests.rs` | The `writer.rs` test module, in a sibling file because it outgrew the module |
| `crates/orange/src/media_diagnostics/operation.rs` | Started/finished timing records around media graph operations |
| `crates/orange/src/media_diagnostics/progress.rs` | RTP/depay/parsed/decoded pad counters, keyframes, queue overruns |
| `crates/orange/src/media_diagnostics/webrtc_monitor.rs` | Periodic sanitized WebRTC statistics and UI/media progress worker |
| `crates/orange/src/auth.rs` | Desktop OAuth polling and atomic `%APPDATA%\orange\session.json` ownership |
| `crates/orange/src/targets.rs` | Capturable-window enumeration/filtering, HWND-to-PID lookup, late-join redraw request |
| `crates/orange/src/text.rs` | System font loading and glyph rasterization for the video overlay |
| `crates/orange/src/connection.rs` | Privacy-safe connection progress stages and failures shared by signalling, WebRTC and playback |
| `crates/orange/src/window/connection_surface.rs` | What the viewer window draws before media arrives: connection stage, close hit test |
| `crates/orange/src/encoder_characterization.rs` | Developer-only `characterize-bitrate` subcommand. It sits outside `media_diagnostics/` on purpose: that tree instruments live sessions, this one benchmarks encoders in a throwaway process and never runs for a user |
| `crates/orange/src/test_support.rs` | `#[cfg(test)]` only: runs a test in a deadline-bounded child process |

## Signal And Entry Source Map

| File | Authoritative responsibility |
| --- | --- |
| `crates/orange-signal/src/lib.rs` | Media-free facade exporting `Signal`, `SignalClient`/`connect`, and `serve` |
| `crates/orange-signal/src/protocol.rs` | Serde-tagged `Signal` protocol and relay-controlled peer routing IDs |
| `crates/orange-signal/src/client.rs` | WebSocket tasks/channels, heartbeat, graceful-close request, task await/reap, abort fallback |
| `crates/orange-signal/src/relay.rs` | Room codes, in-memory rooms, role rules, routing, viewer cap, queues and rate limit |
| `crates/orange-signal/src/relay_tests.rs` | The `relay.rs` test module, in a sibling file because it outgrew the module |
| `crates/orange-signal/src/server.rs` | Axum routes, 512-connection semaphore, OAuth HTTP endpoints, `/ws` |
| `crates/orange-signal/src/auth.rs` | Discord OAuth exchange, pending attempts, opaque sessions cached in memory over a durable store, expiration and capacities |
| `crates/orange-signal/src/store.rs` | Azure Table Storage session rows: Shared Key Lite signing, hashed row keys, upsert/get/delete, optional configuration |
| `crates/orange-relay/src/main.rs` | Production relay entry, `PORT`, Ctrl-C shutdown selection |
| `crates/orange-updater/src/main.rs` | Updater argument parser, parent wait, checksum, silent installer, restart/failure record |

## Friends And Presence

Joining used to require the host to send a room code and the viewer to have it
on the clipboard. The code is still the join capability and the media path is
unchanged; what changed is that the client can now obtain the code itself.

```text
host client            relay                       viewer client
    |                    |                              |
    |-- Host{visible_to} ->  Room{host_id, host_avatar,  |
    |   (Discord ids from |       visible_to, code}      |
    |    preferences.json)|                              |
    |                     |<-- GET /presence?ids= -------|  every 15 s, bearer
    |                     |                              |
    |                     |--- per id, only if the room  |
    |                         lists this caller:         |
    |                         offline | live{code} | full |
    |                                                    |
    |<========== ordinary code join with that code ======|
```

- The roster is client-side, in `preferences.json`. The relay never stores a
  friend graph; it only indexes what a host declares when it opens a room.
- `visible_to` is not an access boundary on the room. Anyone holding the code
  can still join. It decides only who is *handed* the code without being told
  it, so the security model is unchanged and the discovery model is new.
- Offline and "live but not visible to you" are deliberately indistinguishable.
  Reporting them differently would leak the fact hiding was meant to hide.
- `full` is distinct from `live` because a viewer shown a join button that
  immediately fails on `ROOM_VIEWER_CAPACITY` was misled by the button.
- A profile is only refreshed while its owner is streaming: `identify` returns
  the caller's own Discord profile and nothing else, so opening a room is the
  one moment the relay learns a host's name and avatar. The client caches the
  last one it saw so an offline friend still has a face.
- Friendships are created from a code join, in both directions, and only when
  the user accepts. `StreamInfo` carries the host's identity to the viewer and
  `ViewerJoined` carries the viewer's to the host; both are needed, because a
  one-sided add produces a row that reads "Not streaming" while the person is
  streaming. Adding is never automatic: codes get pasted into group chats.

## Host Media Flow

The production client policy prefers H.265 and falls back to H.264 only when no
H.265 factory can statically link to D3D11 input. Frames remain D3D11-backed
through capture, GPU conversion, and hardware encoding:

```text
d3d11screencapturesrc
  -> video/x-raw(memory:D3D11Memory),framerate=...
  -> leaky queue -> d3d11convert -> optional D3D11 scale caps
  -> selected zero-copy encoder:
       mfh265enc (preferred) | nvd3d11h265enc (fallback)
       mfh264enc (fallback)  | nvd3d11h264enc (fallback)
  -> matching h265parse/h264parse -> tee
  -> per-viewer leaky queue -> matching RTP payloader/caps -> webrtcbin
```

- The shared tee is after the parser: capture and hardware encode happen once.
- With no viewers, the shared graph waits in `READY`: capture, encoding, and
  audio stop after the final viewer branch has been removed. A barrier on the
  serial teardown worker finishes that transition before the host handles the
  next join, whose branch is attached before capture restarts.
- Every viewer receives its own payloader, RTP stream, WebRTC peer, offer, ICE, startup keyframe worker, diagnostics handle, and requested pads.
- NACK, periodic keyframes, and redraw requests support recovery and late joins, including windows that are not repainting.
- `--codec auto` tries `mfh265enc`, `nvd3d11h265enc`, `mfh264enc`, then `nvd3d11h264enc`; explicit CLI codec choices retain their fixed factories without fallback.

Audio construction failure disables optional audio. Once linked into the shared pipeline, a later audio error can end the host session.

```text
wasapi2src loopback=true [process-tree scope]
  -> leaky queue -> audioconvert -> audioresample -> stereo 48 kHz
  -> opusenc 128 kbps -> rtpopuspay -> caps -> tee
  -> per-viewer leaky queue -> webrtcbin
```

Window capture supplies its process PID and excludes other applications; whole-screen capture uses PID zero and includes system output audio.

## Watch Media Flow

```text
webrtcbin RTP pad
  -> rtph265depay -> h265parse -> d3d11h265dec
  -> one-buffer leaky queue -> overlaycomposition -> d3d11videosink -> owned HWND

webrtcbin OPUS pad
  -> rtpopusdepay -> opusdec -> audioconvert -> audioresample
  -> volume -> wasapi2sink (fallback: wasapisink)
```

- The receive registry accepts only the first supported video and Opus pads; video consumes the one output target.
- Dynamic receive construction blocks the pad, links and synchronizes all elements, then removes the probe; failure unlinks, sets Null, removes elements, and removes the probe.
- Live receive latency is 100 ms. Video/RTX jitterbuffers drop at the live edge;
  Opus does not silently drop late packets and uses decoder packet-loss concealment.
- Overlay composition cache hits hash borrowed metadata and displayed numeric
  buckets without formatting labels. One connection-stage snapshot supplies
  the key and any labels drawn on a miss; collapsed status skips unused text.

## Ownership And Teardown

- Unique owners initiate teardown: `PlaybackWindow`, `Client`, `Supervisor`, `SignalClient`, `ViewerBranch`, `ReceiveWorkerRegistry`, and job handles.
- Passive `PlaybackWindowHandle` clones may inspect the HWND/alive state and
  overlay, but dropping a handle never destroys the window.
- Playback owners are declared before pipelines. Receive pipelines reach
  `gst::State::Null` before the owner destroys its HWND.
- Client and playback HWNDs are created, messaged, and destroyed on their native
  creator threads. Incomplete native cleanup is fail-fast; detaching a thread
  with unprovable HWND/context ownership is not allowed.
- `SignalClient::close` requests graceful WebSocket close and awaits/reaps tasks; abort handles unfinished work and drop fallback. Other workers are cancelled/disconnected and joined.
- Requested `webrtcbin` and tee pads are owned by branch guards and released.
  Temporary block/idle and bitrate probes are removed on rollback or teardown.
- Teardown combines primary and cleanup errors rather than hiding either one.

## Environment Variables

Application, media-runtime and deployment variables read by shipped code are
listed below. Ordinary Windows location variables (`APPDATA`, `LOCALAPPDATA`,
`ProgramFiles`, `PATH` and `WINDIR`) and Cargo's standard build variables are
omitted. Names beginning `ORANGE_TEST_` and not listed here belong to individual
tests, which use them to mark the child half of a bounded subprocess run; they
are named in the test that owns them.

| Variable | Read by | Meaning |
| --- | --- | --- |
| `ORANGE_SERVER` | `crates/orange/src/main.rs`, `crates/orange-client/src/main.rs` | Relay URL used by the desktop client and the CLI default for `login`, `host` and `watch`. An explicit CLI `--server` wins |
| `GSTREAMER_1_0_ROOT_MSVC_X86_64` | `crates/orange-client/src/supervisor.rs` | Development GStreamer root. The bundled runtime wins when present; this is the next lookup before installed locations |
| `ORANGE_UI_PID` | `crates/orange/src/peer/host.rs` | Client PID, so whole-screen capture can exclude the client's own cues. Set by the supervisor |
| `ORANGE_MEDIA_DIAGNOSTICS` | `crates/orange-client/src/supervisor.rs`, `crates/orange/src/media_diagnostics/writer.rs` | Directory for the JSONL diagnostic log. The client supplies its default to media children; absent in a media child disables the log |
| `ORANGE_BUILD_ID` | `crates/orange-client/src/update.rs` and `crates/orange-client/src/supervisor.rs` (compile time), `crates/orange/src/media_diagnostics/writer.rs` | Release commit baked into the client and stamped on diagnostic records. It is what a log should be read against |
| `ORANGE_RUN_ID`, `ORANGE_DEVICE_ID`, `ORANGE_TEST_PROFILE` | `crates/orange/src/media_diagnostics/writer.rs` | Optional run, machine and test-profile metadata stamped on diagnostic records |
| `ORANGE_RTP_BUFFER_MODE` | `crates/orange/src/webrtc/transport.rs` | `none` disables jitterbuffer smoothing and RTCP sync. A latency experiment, not a supported setting |
| `ORANGE_AV1_DECODER` | `crates/orange/src/webrtc/receive.rs` | `software` selects dav1d for live AV1 diagnostic comparisons. File output and every other value keep the D3D11 decoder |
| `ORANGE_UPDATE_CHANNEL` | `crates/orange-client/src/update.rs` (compile time) | `beta` enables update checks. Baked in by `package.ps1`, so a local build never checks |
| `ORANGE_TABLE_ACCOUNT`, `ORANGE_TABLE_KEY`, `ORANGE_TABLE_NAME` | `crates/orange-signal/src/store.rs` | Azure Table Storage for durable sessions. All three absent runs the relay with memory-only sessions |
| `DISCORD_CLIENT_ID`, `DISCORD_CLIENT_SECRET`, `DISCORD_REDIRECT_URI` | `crates/orange-signal/src/auth.rs` | Discord OAuth. Absent leaves identity off and peers anonymous |
| `PORT` | `crates/orange-relay/src/main.rs` | Listen port, injected by Container Apps. Defaults to 9000 |

## Compatibility Contracts

| Contract | Authority |
| --- | --- |
| Signal JSON tags, fields, defaults, and peer stamping | `crates/orange-signal/src/protocol.rs` |
| Client window discovery JSON from `orange list --json` | producer: `crates/orange/src/main.rs`; consumer: `crates/orange-client/src/supervisor.rs` |
| Client child commands/flags: `list --json`; `login --server`; `host --hwnd --server --codec --scale --fps --visible-to`; `watch --code --server --cascade --profile` | producer: `crates/orange-client/src/supervisor.rs`; consumer: `crates/orange/src/main.rs` |
| `GET /presence?ids=` request, bearer auth, and `{friends:[{id,name,avatar_url,state,code}]}` reply | producer: `crates/orange-signal/src/server.rs`; consumer: `crates/orange-client/src/presence.rs` |
| Host stdout markers `Share this code:` and `[host-status] <json>`, whose `joined` event carries `id` and `avatar_url` | producer: `crates/orange/src/peer/host.rs`; consumer: `crates/orange-client/src/supervisor.rs` |
| Automatic bitrate cap marker `[quality-status] <json>` | producer: `crates/orange/src/main.rs`; consumer: `crates/orange-client/src/supervisor.rs` |
| Watch stdout markers `[watch-status] ended` and `[watch-host] <json>` | producer: `crates/orange/src/peer/watch.rs`; consumer: `crates/orange-client/src/supervisor.rs` |
| `ORANGE_TABLE_ACCOUNT`, `ORANGE_TABLE_KEY`, `ORANGE_TABLE_NAME`, and the session row schema | producer: `deploy/azure.ps1`; consumer: `crates/orange-signal/src/store.rs` |
| `ORANGE_UI_PID`, so whole-screen capture can exclude the client's own cues | producer: `crates/orange-client/src/supervisor.rs`; consumer: `crates/orange/src/peer/host.rs` |
| Session file `%APPDATA%\orange\session.json` | writer: `crates/orange/src/auth.rs`; reader: `crates/orange-client/src/session.rs` |
| Preferences `%APPDATA%\orange\preferences.json` | `crates/orange-client/src/session.rs` |
| Beta manifest schema/host/name/hash | producer: `publish-beta.ps1`; consumer: `crates/orange-client/src/update.rs` |
| Updater flags `--installer --sha256 --parent --install-dir` | producer: `crates/orange-client/src/update.rs`; consumer: `crates/orange-updater/src/main.rs` |
| RTP video payload 96, RTX 97, Opus 111, clocks and 100 ms receive latency | `crates/orange/src/webrtc/transport.rs` |
| Local diagnostic JSONL fields and `ORANGE_*` metadata | producer: `crates/orange/src/media_diagnostics/writer.rs`; consumer: a human, via Settings -> Diagnostics -> Open folder |

## Where Do I Change...?

| Change | Start here |
| --- | --- |
| CLI flags/defaults and client child invocations | `crates/orange/src/main.rs`, `crates/orange-client/src/supervisor.rs` |
| Capture elements, encoder choice, Opus send chain | `crates/orange/src/pipeline.rs` |
| Host fan-out, late join, keyframe behavior | `crates/orange/src/peer/host.rs`, `crates/orange/src/peer/host_branch.rs` |
| Viewer negotiation and session lifetime | `crates/orange/src/peer/watch.rs` |
| Decoder, depayloader, sinks, receive rollback | `crates/orange/src/webrtc/receive.rs` |
| RTP payloads, jitterbuffer latency/drop policy | `crates/orange/src/webrtc/transport.rs` |
| Native viewer behavior and HWND lifetime | `crates/orange/src/window.rs`, `crates/orange/src/window/native.rs` |
| Overlay behavior/layout | `crates/orange/src/overlay.rs`, `crates/orange/src/overlay/raster.rs`, `crates/orange/src/overlay/gst.rs` |
| Client screens | `crates/orange-client/src/view/` — one file per screen |
| Friends list, presence polling, add/remove | `crates/orange-client/src/presence.rs`, `crates/orange-client/src/view/home.rs`, `crates/orange-client/src/main.rs` |
| Who may discover a stream | `visible_to` on `Signal::Host`, set in `crates/orange-client/src/main.rs`, filtered in `crates/orange-signal/src/relay.rs` |
| Client colors/components | `crates/orange-client/src/ui/theme.rs`, `crates/orange-client/src/ui/controls.rs` |
| Logo, glow, ambient grid | `crates/orange-client/src/ui/mark.rs`, `crates/orange-client/src/ui/decor.rs` |
| Client sound cues | `crates/orange-client/src/sound.rs` |
| Client quality tiers and child log parsing | `crates/orange-client/src/supervisor.rs` |
| Frame-rate options and the 120 fps cap | `crates/orange-client/src/supervisor.rs` (`FRAME_RATES`), `crates/orange/src/pipeline.rs` (`MAX_FPS`) |
| Session/preferences persistence | `crates/orange/src/auth.rs`, `crates/orange-client/src/session.rs` |
| Signal wire format | `crates/orange-signal/src/protocol.rs` |
| Relay room policy and limits | `crates/orange-signal/src/relay.rs`, `crates/orange-signal/src/server.rs` |
| Discord OAuth/session policy | `crates/orange-signal/src/auth.rs` |
| Session durability, table schema, request signing | `crates/orange-signal/src/store.rs`, `deploy/azure.ps1` |
| Local media diagnostic records | `crates/orange/src/media_diagnostics/writer.rs` |
| Update manifest/client handoff | `publish-beta.ps1`, `crates/orange-client/src/update.rs`, `crates/orange-updater/src/main.rs` |
| Installer contents/prerequisites | `package.ps1`, `packaging/windows/orange.iss` |
| Which GStreamer elements ship in the installer | `packaging/windows/stage-gstreamer.ps1` |
| Release procedure (bump, test, commit, push, publish) | `ship.ps1` |
| Pre-push gate | `packaging/hooks/pre-push.ps1` |
| Continuous integration | `.github/workflows/ci.yml` |
| Application icon or logo | `assets/` |
| Azure single-replica deployment | `deploy/azure.ps1` |

## Validation

Run from a PowerShell prompt at the repository root:

```powershell
. .\dev.ps1

# Target the package changed during development.
cargo test --locked -p orange

# Full local source gates.
cargo test --locked --workspace --all-features
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
cargo build --locked --release --workspace --all-features

# Builds and validates the per-user installer; requires Inno Setup 6.
.\package.ps1
```

The fast subset of the source gates runs automatically on `git push` once
`git config core.hooksPath packaging/hooks` is set. See
`packaging/hooks/pre-push.ps1` for what it covers and what it deliberately
leaves to the full matrix above. `.github/workflows/ci.yml` runs the same
subset on GitHub so the gate holds whether or not that hook is installed; it
cannot cover `orange` or `orange-client`, which need GStreamer and Win32.

Direct PowerShell contract tests do not publish or deploy:

```powershell
.\deploy\test-azure-script.ps1
.\packaging\windows\test-installer.ps1
.\packaging\windows\test-package-provenance.ps1
.\packaging\windows\test-beta-publish.ps1
```

External gates are installed interactive media acceptance on required GPU
vendors, direct two-machine WAN, deployment, publication, and signing.
`deploy/azure.ps1` uses a cloud source build, so local Docker availability is
not a source-level result.

## Deployment Model And Boundaries

- Azure is deliberately pinned to one always-on replica in `deploy/azure.ps1`.
- Sessions are durable in Azure Table Storage, so a deploy no longer signs users out. Memory is a read-through cache in front of it; the store is consulted only on a miss. Login and durable restoration share the same 4,096-entry eviction policy; replacing a cached token neither evicts another entry nor renews its original expiration.
- Rooms and pending OAuth attempts are still in memory. A revision switch, deploy, or restart interrupts every active room. There is no horizontal-scaling claim: rooms are per-process, so two replicas behind one ingress could put host and viewer on different instances.
- Unreachable session storage degrades rather than fails: a lookup returns nothing but is never treated as proof that a session is absent, and a login that cannot be persisted still succeeds for the life of the process.
- The process accepts at most 512 concurrent WebSocket connections and each room accepts at most 16 viewers.
- Each peer has a 64-message outbound queue. Inbound signalling permits 256 text messages per 10-second fixed window; WebSocket messages cap at 64 KiB.
- Auth memory is bounded to 1,024 pending attempts and 4,096 cached sessions, with 10-minute pending and 30-day session lifetimes. The session bound is a cache limit, not a user limit: eviction drops the memory copy and the row survives.
- Presence is polled over HTTP rather than pushed over the WebSocket, so it costs nothing against the 512-connection cap. The price is up to `presence::INTERVAL` of latency on a friend going live.
- Direct ICE and public Google STUN are configured; no TURN configuration exists, so TURN-required networks are unsupported.
- HTTPS host restrictions constrain download origin and the manifest SHA-256 detects corruption, but neither authenticates the publisher if the origin or manifest is compromised. Production promotion remains blocked on a trusted Authenticode certificate.
