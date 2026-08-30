<p align="center">
  <img src="crates/orange-tray/logo.png" width="88" alt="orange">
</p>

<h1 align="center">orange</h1>

<p align="center">
  Low-overhead window streaming for friends. Share a game window at high
  bitrate without the compression Discord puts on it, and without costing
  yourself FPS.
</p>

## Status

Nine milestones done, and the tray UI is usable. Everything below has only ever
run on a single machine: the two-machine path across the internet is the next
real test, and the biggest unknown.

| # | Milestone | State |
| --- | --- | --- |
| 1 | Window enumeration, GPU capture, hardware encode | **done** |
| 2 | WebRTC transport, no transcode | **done** (loopback) |
| 3 | Signalling relay, host/watch as separate processes | **done** (one machine) |
| 4 | Multiple simultaneous viewers, single encode | **done** |
| 5 | Relay deployed to Azure | **done** |
| 6 | Viewer window: borderless + rounded, video embedded | **done** |
| 7 | Per-process game audio | **done** |
| 8 | Overlay controls on the video | **done** |
| 9 | Discord identity | **done** |
| 10 | Tray UI (window picker, quality, share code) | **done** |
| 11 | Two machines across the internet | next |
| 12 | Installer, autostart | |

## Identity

```
tray → browser → Discord consent
                     ↓
             relay /auth/callback     client_secret lives ONLY here
                     ↓ exchange code, GET /users/@me
               session token
                     ↑
tray polls /auth/poll?state=… ──┘     no local server, no fixed port
```

```powershell
orange login     # opens the browser, stores the session
orange logout
```

Two decisions worth keeping:

- **The desktop app never holds `client_secret`.** Anything shipped to a user's
  machine can be extracted from it, so the code-for-token exchange happens on
  the relay and the app only ever receives an opaque session token.
- **Polling, not a loopback redirect.** Discord requires redirect URIs to match
  exactly, which would pin the app to a hardcoded port that may be in use.
  The browser lands back on the relay; the app polls with a nonce it generated.

Once signed in, hosts see `Ale joined (2 watching)` instead of `cd6da841`, and
viewers see whose stream they opened.

**Identity is not an access boundary.** Possession of the room code still grants
access; logging in only attaches a name. Guild-based authorisation would change
that, and needs the `guilds` scope.

### Friends lists

Discord's `relationships.read` scope exists but is **gated behind Social SDK
approval**. The approval-free equivalent is `identify` + `guilds`: match users
who share a Discord server. Functionally the same for a group of friends.

### Relay configuration

```powershell
az containerapp secret set --name orange-relay --resource-group orange-rg `
  --secrets discord-secret=<SECRET>

az containerapp update --name orange-relay --resource-group orange-rg `
  --set-env-vars DISCORD_CLIENT_ID=<ID> `
                 DISCORD_CLIENT_SECRET=secretref:discord-secret `
                 DISCORD_REDIRECT_URI=https://<fqdn>/auth/callback
```

Unconfigured, the relay still works for anonymous peers and returns a readable
503 from `/auth/start`, so local development needs no credentials.

**Sessions are in memory.** Every relay restart or redeploy signs everyone out.
That needs a datastore before this goes to real users.

## Audio is scoped to the game

`wasapi2src` can record a single process tree rather than the whole output
device:

```
wasapi2src loopback=true loopback-mode=include-process-tree loopback-target-pid=<game>
```

**Your voice chat, music and notification sounds never enter the stream.** The
PID comes from the captured window automatically, so there is nothing to
configure. Opus at 128 kbps stereo is transparent for games and a rounding
error next to the video bitrate.

Pass `--no-audio` to disable it. Audio failing never blocks the stream — if the
process makes no sound or capture fails, video continues and the reason is
logged.

## Measured, not assumed

On an RTX 4080 SUPER / i9-14900K, capturing a live UE5 game:

| | |
| --- | --- |
| Resolution | 3840x2160 @ 60fps |
| Codec | AV1, NVENC, D3D11 zero-copy |
| Bitrate | ~29 Mbps |
| CPU | **1.03s over 11.5s wall** — ~9% of one core |
| In-game FPS cost | **none observed** |

For comparison, Discord's free tier caps at 1080p60 with far heavier
compression. This is not an incremental improvement.

## Why it is cheap

Frames never leave VRAM:

```
d3d11screencapturesrc   -> memory:D3D11Memory   (Windows Graphics Capture)
d3d11convert            -> scale/convert on GPU
nvd3d11av1enc           -> NVENC, a dedicated ASIC, not CUDA cores
```

**Inserting any element that forces a download to system memory
(`videoconvert`, `videoscale`, most CPU filters) destroys this property.** If a
change makes CPU usage jump, that is the first thing to check.

## Build

```powershell
winget install gstreamerproject.gstreamer Rustlang.Rustup pkgconf.pkgconf
winget install Microsoft.VisualStudio.2022.BuildTools --override "--quiet --wait --add Microsoft.VisualStudio.Workload.VCTools"

. .\dev.ps1      # sets PKG_CONFIG_PATH and PATH - note the leading dot
cargo build
```

## Install

The beta uses a one-click per-user installer. Open
`orange-setup-0.2.0-beta.2.exe`; it installs prerequisites when needed,
installs Orange under `%LOCALAPPDATA%\Programs\orange`, creates a Start menu
shortcut, and opens the app. There are no destination, shortcut, or completion
pages to step through.

Installed beta builds check the public update channel at startup and every six
hours. When a newer beta is available, Orange shows an update banner. Clicking
`Update now` downloads and verifies the installer, closes Orange, applies the
update silently, and reopens the app. Failed checks and downloads do not stop
streaming.

The first beta is not Authenticode-signed, so Windows SmartScreen may warn on
the initial install. HTTPS host pinning and SHA-256 verification protect update
downloads, but production promotion remains blocked on obtaining a trusted
code-signing certificate.

Build the friend-facing Windows installer with:

```powershell
.\package.ps1
```

The result is `dist\orange-setup-<version>.exe`. It installs `orange` for the
current user and creates a Start menu shortcut. If the required GStreamer media
runtime is missing, setup downloads the pinned official x64 runtime from the
GStreamer project and verifies its SHA-256 hash before installing it. Rust,
Visual Studio, and the source tree are not needed on the receiving machine.

The current build targets 64-bit Windows 10/11. Hosting requires an NVIDIA GPU
with AV1 NVENC support; watching requires hardware AV1 decode exposed through
the Windows D3D11 media stack.

### Alpha testing

Alpha testers download
[`orange-alpha-launcher.zip`](https://orangealpha0d8d5893e69a3.blob.core.windows.net/releases/orange-alpha-launcher.zip)
once, extract it, and run `orange-alpha.cmd`. The launcher checks the
channel manifest, verifies the build's SHA-256 hash, keeps the previous build
for rollback, and starts the selected version. No installer is replaced.

Each launch writes diagnostics under `%LOCALAPPDATA%\Orange Alpha\diagnostics`
and retries completed archives from `%LOCALAPPDATA%\Orange Alpha\pending`.
After Orange closes, signed-in alpha testers automatically upload the archive
to the relay. Upload failure does not block Orange and retries on the next run.

Uploaded archives contain build/profile identifiers and Orange's structured
media counters and timings. They do not contain video, audio, Discord tokens,
authorization headers, room codes, SDP, ICE candidates, window titles, or
process names. The server stores a one-way digest rather than the Discord ID.

Maintainers build and validate the next alpha locally with:

```powershell
.\alpha\publish-alpha.ps1
```

After the commit is on `origin/main`, publication is explicit:

```powershell
.\alpha\publish-alpha.ps1 -Publish
```

## Transport: why `webrtcbin`, not `webrtcsink`

`webrtcsink` is the friendlier element — it handles negotiation and codec
selection for you — but **it owns the encoder and expects raw video**. Using it
would re-encode frames we already encoded on the GPU, throwing away the reason
this project is cheap.

`webrtcbin` accepts RTP-payloaded, already-encoded media, so NVENC output goes
straight onto the wire:

```
nvd3d11av1enc -> av1parse -> rtpav1pay -> webrtcbin
                                             |
webrtcbin -> rtpav1depay -> av1parse -> d3d11av1dec -> d3d11videosink
```

The price is writing signalling ourselves. `orange loopback` runs both peers in
one process and passes SDP by direct function call, which exercises the whole
media path with no network code in the way.

## Usage

```powershell
orange list
# HWND                SIZE  PROCESS                          TITLE
# 395876         2560x1440  MortalShell2-Win64-Shipping.exe  MortalShell2

# capture + encode only
orange record --hwnd 395876 --codec av1 --bitrate 25000 --scale 1920x1080 --seconds 8 --out test.mkv

# capture -> encode -> WebRTC -> decode -> render
orange loopback --hwnd 395876 --scale 1280x720 --bitrate 8000 --show
```

Both subcommands are diagnostics: `record` isolates the capture half, `loopback`
adds transport. When a real stream misbehaves, they tell you which half is at
fault.

## Streaming between machines

```powershell
orange serve                              # the relay (one instance, anywhere reachable)
orange host --hwnd 395876 --scale 1920x1080 --bitrate 25000
#   Share this code:  BC2-VH3
orange watch --code BC2-VH3               # on a friend's machine
```

Point both ends at the same relay with `--server ws://host:9000`.

### The relay carries no video

It knows nothing about media. It matches peers by room code and forwards a few
kilobytes of SDP and ICE, then gets out of the way — video goes directly peer
to peer. That is what keeps hosting costs near zero.

### How many viewers?

The relay is not the limit. It handles thousands of concurrent connections on
the cheapest tier, because a whole session costs it perhaps 10-20 KB of
handshake plus an idle socket.

**The limit is the host's upload bandwidth.** The window is captured and encoded
*once* — a `tee` fans the encoded stream out — so GPU and CPU cost stay flat no
matter how many people watch. Bandwidth does not:

| Bitrate | 2 viewers | 4 viewers | 8 viewers |
| --- | --- | --- | --- |
| 25 Mbps (1440p60, excellent) | 50 Mbps | 100 Mbps | 200 Mbps |
| 8 Mbps (1080p60, good) | 16 Mbps | 32 Mbps | 64 Mbps |
| 4 Mbps (720p60, fine) | 8 Mbps | 16 Mbps | 32 Mbps |

On 100 Mbps upload that is roughly **4 viewers at 25 Mbps, or a dozen at 8
Mbps**. Lower the bitrate as the audience grows; the tray UI should make that
tradeoff visible rather than hiding it.

Beyond that, an SFU would be needed — the host uploads once and a server fans
out — but that server *does* carry video, at roughly 9 GB per hour per viewer
in egress. That is a completely different cost structure and not worth it for a
group of friends.

### One replica, on purpose

Rooms live in memory, so the deployment pins `min-replicas = max-replicas = 1`.
Two replicas behind one ingress could put a host and viewer on different
instances, and they would never find each other. Scaling horizontally would
need shared state (Redis or similar) — unnecessary at this size, but it is why
the relay is a single point of failure.

**The room code is the only credential.** Anyone you give it to can watch, and
can pass it on. That is the deliberate cost of "no accounts, no logins".

Public STUN (`stun.l.google.com:19302`) is used for address discovery. Peers
that cannot hole-punch will need a TURN server, which *does* relay video and
therefore costs real bandwidth.

## Deploying the relay

```powershell
az login
.\deploy\azure.ps1
#   wss://orange-relay.<region>.azurecontainerapps.io
```

Azure Container Apps terminates TLS and provides an HTTPS hostname, so clients
get `wss://` with no certificate work. The relay crate deliberately has no
GStreamer dependency — three direct dependencies total — so the container stays
small.

## Gotchas found the hard way

- **Backslashes are escape characters in GStreamer's parse syntax.** Building a
  pipeline string with a Windows path in it silently mangles the path and you
  get an empty file with no error. Sinks are constructed programmatically for
  this reason.
- **WGC only produces frames when the window redraws.** A minimised or idle
  window starves the pipeline and it will hang forever. Anything waiting on
  frames needs a timeout.
- **Framerate caps must sit directly after the source.** `d3d11convert` does
  not do framerate conversion, so requesting a rate downstream of it fails to
  negotiate.
- `BOOL` lives in `windows::core`, not `windows::Win32::Foundation`.

## License

MIT. GStreamer is LGPL-2.1 and linked dynamically; GPUI is Apache-2.0.
