<p align="center">
  <img src="crates/orange-tray/logo.png" width="88" alt="orange">
</p>

<h1 align="center">orange</h1>

<p align="center">
  Low-overhead Windows game and window streaming for friends, with direct
  peer-to-peer media and a small signalling relay.
</p>

## Status

Ten core product milestones are implemented:

| # | Milestone | State |
| --- | --- | --- |
| 1 | Window enumeration, D3D11 capture, hardware encoding | done |
| 2 | WebRTC transport without a production-path re-encode | done |
| 3 | Signalling relay with separate host/watch processes | done |
| 4 | Multiple viewers sharing one host encode | done |
| 5 | Single-replica Azure relay deployment | done |
| 6 | Native borderless viewer window with embedded video | done |
| 7 | Window process-tree audio and whole-screen system audio | done |
| 8 | GPU-composited viewer controls | done |
| 9 | Optional Discord identity | done |
| 10 | Tray UI, installer, and verified beta updates | done |

Autostart is not implemented. Current local installed acceptance covers H.265
streaming checks, preview, and file output. Two-machine WAN behavior, including
networks that require TURN, remains an external acceptance gate.

## Architecture

See [ARCHITECTURE.md](ARCHITECTURE.md) for process boundaries, crate and source
maps, media flows, teardown rules, compatibility contracts, limits, and the
authoritative validation commands.

## Identity And Access

Discord identity is optional in the signalling protocol and CLI. It adds names
and avatars and authorizes alpha diagnostic uploads; anonymous peers can still
host and watch when the relay is not configured for Discord.

```text
tray -> browser -> Discord consent
                     |
                     v
             relay /auth/callback       client_secret stays on relay
                     |
                     v
               session token
                     ^
                     |
tray polls /auth/poll?state=...          no local callback port
```

```powershell
orange login
orange logout
```

The desktop receives an Orange session token, never the Discord client secret.
The browser callback lands on the relay because Discord redirect URIs must match
exactly; the desktop polls with the generated state value.

Identity is not authorization to a room. The room code is the access credential:
anyone who has it can attempt to join and can share it with someone else.

Relay OAuth sessions are in memory. A relay restart, deploy, or Container Apps
revision switch signs users out and interrupts active rooms.

### Relay OAuth configuration

```powershell
az containerapp secret set --name orange-relay --resource-group orange-rg `
  --secrets discord-secret=<SECRET>

az containerapp update --name orange-relay --resource-group orange-rg `
  --set-env-vars DISCORD_CLIENT_ID=<ID> `
                 DISCORD_CLIENT_SECRET=secretref:discord-secret `
                 DISCORD_REDIRECT_URI=https://<fqdn>/auth/callback
```

Without this configuration, anonymous signalling remains available and
`/auth/start` returns a readable service-unavailable response.

## Media Path

The production default is cross-vendor H.265 through Windows Media Foundation:

```text
d3d11screencapturesrc -> d3d11convert -> mfh265enc -> h265parse
  -> rtph265pay -> webrtcbin ===== peer to peer ===== webrtcbin
  -> rtph265depay -> h265parse -> d3d11h265dec
  -> overlaycomposition -> d3d11videosink
```

The host captures and hardware-encodes once. A tee creates an RTP/WebRTC branch
per viewer, so GPU encode cost stays largely flat while host upload bandwidth
grows once per viewer. `webrtcbin` accepts encoded RTP; `webrtcsink` would own
an encoder and require raw video, causing another encode.

D3D11 frames stay GPU-resident through capture, conversion, and the hardware
encoder. CPU conversion elements such as `videoconvert` or `videoscale` in that
production chain would change its performance profile.

AV1 with NVIDIA encoding and H.264 remain explicit CLI diagnostic/development
options. They are not the production default.

### Audio scope

For a window, `wasapi2src` includes only the owning process tree, excluding
voice chat, music, notifications, and other applications:

```text
wasapi2src loopback=true loopback-mode=include-process-tree \
  loopback-target-pid=<game> -> opusenc -> rtpopuspay
```

Whole-screen sharing includes system output audio. Pass `--no-audio` to disable
audio. Audio capture failure does not block video.

### Historical diagnostic measurement

An earlier AV1/NVIDIA RTX 4080 SUPER diagnostic run captured a live UE5 game at
3840x2160 60 fps and about 29 Mbps. It measured about 1.03 CPU-seconds over
11.5 seconds wall time (roughly 9% of one core) with no in-game FPS loss
observed in that run. This is historical characterization, not the active codec
or a cross-vendor performance guarantee.

## Install

The beta uses a one-click per-user installer. Open
`orange-setup-<version>.exe`; it installs Orange under
`%LOCALAPPDATA%\Programs\orange`, creates a Start menu shortcut, installs the
pinned GStreamer runtime per-user when missing, and opens the app. Rust, Visual
Studio, and the source tree are not required on the receiving machine.

Installed beta builds check the public update channel at startup and every six
hours. `Update now` downloads from the fixed release host, enforces size and
manifest rules, verifies SHA-256, hands off to `orange-updater.exe`, closes the
tray and media children, applies the installer silently, and reopens the tray.
Failed checks and downloads do not stop streaming.

The installer is not yet Authenticode-signed. HTTPS host restrictions and
SHA-256 protect update integrity, but public production promotion remains
blocked on obtaining a trusted external code-signing certificate.

Build the installer with:

```powershell
.\package.ps1
```

The output is `dist\orange-setup-<version>.exe`. The current target is 64-bit
Windows 10/11.

### Alpha testing

Alpha testers download
[`orange-alpha-launcher.zip`](https://orangealpha0d8d5893e69a3.blob.core.windows.net/releases/orange-alpha-launcher.zip)
once, extract it, and run `orange-alpha.cmd`. The launcher verifies the channel
manifest and archive SHA-256, keeps the previous build for rollback, and starts
the selected version.

Diagnostics are written under `%LOCALAPPDATA%\Orange Alpha\diagnostics`.
Signed-in testers upload completed ZIPs after Orange exits; failed uploads are
retried later. Archives contain structured counters, timings, and build/profile
identifiers, not media, tokens, authorization headers, room codes, SDP, ICE,
window titles, or process names. Storage uses a one-way digest of the Discord ID.

Build and validate an alpha locally with:

```powershell
.\alpha\publish-alpha.ps1
```

Publication is explicit and requires the commit to exist on `origin/main`:

```powershell
.\alpha\publish-alpha.ps1 -Publish
```

## Build

```powershell
winget install gstreamerproject.gstreamer Rustlang.Rustup pkgconf.pkgconf
winget install Microsoft.VisualStudio.2022.BuildTools --override "--quiet --wait --add Microsoft.VisualStudio.Workload.VCTools"

. .\dev.ps1
cargo build --locked
```

Run the complete validation matrix from [ARCHITECTURE.md#validation](ARCHITECTURE.md#validation).

## Usage

```powershell
orange list
orange list --json

# Capture and encode only; H.265 is the default.
orange record --hwnd 395876 --bitrate 18000 --scale 2560x1440 --seconds 8 --out test.mkv

# Capture, WebRTC transport, decode, and local render.
orange loopback --hwnd 395876 --scale 1280x720 --bitrate 8000 --show

# Local relay and two peer processes.
orange serve
orange host --hwnd 395876 --scale 1920x1080 --bitrate 8000
# Share this code: BC2-VH3
orange watch --code BC2-VH3
```

Point host and watch at another relay with `--server ws://host:9000/ws` or set
`ORANGE_SERVER`. `record` isolates capture/encode; `loopback` adds the media
transport without the network relay.

The CLI defaults to H.265 at 25 Mbps and can choose another bitrate. The tray
offers fixed H.265 tiers: 4 Mbps at 720p, 8 Mbps at 1080p, and 18 Mbps at 1440p.

## Relay Limits And Deployment

The relay forwards signalling only. SDP and ICE pass through it; media remains
peer to peer. The process is deliberately bounded to 512 concurrent WebSocket
connections and 16 viewers per room. It also uses bounded outbound queues and
an inbound signalling rate limit; these are fixed process limits.

Host upload remains the practical media limit because every viewer receives a
separate peer-to-peer stream:

| Tray tier | 2 viewers | 4 viewers | 8 viewers |
| --- | --- | --- | --- |
| 18 Mbps | 36 Mbps | 72 Mbps | 144 Mbps |
| 8 Mbps | 16 Mbps | 32 Mbps | 64 Mbps |
| 4 Mbps | 8 Mbps | 16 Mbps | 32 Mbps |

Rooms and auth sessions are process-local memory. `deploy/azure.ps1` therefore
pins `min-replicas = max-replicas = 1`. Deploying or restarting that one replica
drops active rooms and signs users out. Horizontal scaling would require shared
room and auth state; Orange makes no current multi-replica claim.

Public STUN (`stun.l.google.com:19302`) is configured for address discovery.
No TURN server is bundled. Peers that cannot establish a direct path need a
separately operated TURN service, which carries media and incurs bandwidth cost.

Deploy the relay with a cloud source build:

```powershell
az login
.\deploy\azure.ps1
```

The script uses `az containerapp up --source .`; local Docker availability is
not evidence for or against the source or deployment design.

## Gotchas

- Backslashes are escapes in GStreamer parse strings. File sinks are created
  programmatically, and preview paths are normalized.
- Windows Graphics Capture can stop producing frames when a window is idle or
  minimized. Timed diagnostics and late-join redraw requests account for this.
- Capture framerate caps belong directly after `d3d11screencapturesrc` because
  `d3d11convert` does not perform framerate conversion.
- Pipelines must reach `Null` before their native playback HWND owner drops.

## License

MIT. GStreamer is LGPL-2.1 and dynamically linked; GPUI is Apache-2.0.
