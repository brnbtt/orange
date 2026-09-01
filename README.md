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
| 10 | Tray UI, installer, beta update checks and SHA-256 download verification | done |

Autostart is not implemented. Current local installed acceptance covers H.265
streaming checks, preview, and file output. Direct two-machine WAN remains an
external acceptance gate. Networks that require TURN are currently unsupported.

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

The tray production policy prefers zero-copy H.265. It first tries Windows
Media Foundation, then NVIDIA, and falls back to a compatible zero-copy H.264
encoder when neither H.265 encoder can accept D3D11 textures:

```text
d3d11screencapturesrc -> d3d11convert -> one selected branch:
  -> mfh265enc (preferred) ------> h265parse -> rtph265pay
  -> nvd3d11h265enc (fallback) --> h265parse -> rtph265pay
  -> mfh264enc (fallback) --------> h264parse -> rtph264pay
  -> nvd3d11h264enc (fallback) ---> h264parse -> rtph264pay
  -> webrtcbin ===== peer to peer ===== webrtcbin
  -> matching RTP depayloader -> parser -> D3D11 decoder
  -> overlaycomposition -> d3d11videosink
```

The host captures and hardware-encodes once. A tee creates an RTP/WebRTC branch
per viewer, so GPU encode cost stays largely flat while host upload bandwidth
grows once per viewer. `webrtcbin` accepts encoded RTP; `webrtcsink` would own
an encoder and require raw video, causing another encode.

D3D11 frames stay GPU-resident through capture, conversion, and the hardware
encoder. CPU conversion elements such as `videoconvert` or `videoscale` in that
production chain would change its performance profile.

The CLI default remains explicit Media Foundation H.265. `--codec auto` uses
the tray policy; explicit `h265`, `h264`, and `av1` choices never fall back to a
different factory or codec.

### Audio scope

For a window, `wasapi2src` includes only the owning process tree, excluding
voice chat, music, notifications, and other applications:

```text
wasapi2src loopback=true loopback-mode=include-process-tree \
  loopback-target-pid=<game> -> opusenc -> rtpopuspay
```

Whole-screen sharing includes system output audio. Pass `--no-audio` to disable
audio. Failure while constructing optional audio disables it and leaves video
running; a later audio error in the shared host pipeline can end the session.

### Historical diagnostic measurement

An earlier AV1/NVIDIA RTX 4080 SUPER diagnostic run captured a live UE5 game at
3840x2160 60 fps and about 29 Mbps. It measured about 1.03 CPU-seconds over
11.5 seconds wall time (roughly 9% of one core) with no in-game FPS loss
observed in that run. This is historical characterization, not the active codec
or a cross-vendor performance guarantee.

## Install

The beta uses a one-click per-user installer. Open
`orange-setup-<version>.exe`; it installs Orange under
`%LOCALAPPDATA%\Programs\orange`, creates a Start menu shortcut, and opens the
app. Rust, Visual Studio, and the source tree are not required on the receiving
machine.

Setup never reaches the network. Earlier builds downloaded the official
GStreamer runtime during install, which is a 504 MB file from a host with no CDN
behind it, so a first install could take several minutes or fail outright.
Orange loads about 38 MB of that runtime, and `package.ps1` now stages exactly
that slice into the installer: it asks GStreamer which plugin provides each
element the pipelines need, walks the import tables to collect their dependent
DLLs, and refuses to build unless every element resolves from the staged tree
alone. The result installs offline from a single 17 MB file.

Installed beta builds check the public update channel at startup and every six
hours. `Update now` downloads from the fixed release host, enforces size and
manifest rules, verifies SHA-256, hands off to `orange-updater.exe`, closes the
tray and media children, applies the installer silently, and reopens the tray.
Failed checks and downloads do not stop streaming.

The installer is not yet Authenticode-signed. HTTPS host restrictions constrain
the download origin and the manifest SHA-256 detects corruption, but neither
authenticates the publisher if the origin or manifest is compromised. Public
production promotion remains blocked on a trusted Authenticode certificate.

Build the installer with:

```powershell
.\package.ps1
```

The output is `dist\orange-setup-<version>.exe`. The current target is 64-bit
Windows 10/11.

### Releasing

Ship a new release with one command. It bumps the version, runs the full test
suite, commits, pushes, builds the installer, and publishes:

```powershell
.\ship.ps1 -Notes "Fixes audio dropping out when the game loses focus."
.\ship.ps1 -Minor -Notes "Adds a settings screen."
```

Everything that can fail cheaply runs first, so a failure never leaves you with
a committed-and-pushed version bump to unwind. `-Notes` is required: users see
that text in the update banner.

Versions are plain `MAJOR.MINOR.PATCH`. There is no `-beta` suffix, because the
leading `0` already means "unstable" in semver and the update manifest carries
`"channel": "beta"` separately. `.\ship.ps1` bumps the patch by default, since
most releases are fixes; `-Minor` is for a release that adds something, and
`-Major` is how `1.0.0` arrives once Orange is production ready. `-Version` sets
the number outright for anything those cannot express.

If the publish itself fails partway, do **not** re-run `ship.ps1` — it would
bump the version again. Re-run the publisher directly instead; it is idempotent
and converges on the same commit:

```powershell
.\publish-beta.ps1 -Publish -Notes "<same notes>"
```

Build an installer locally without publishing anything:

```powershell
.\package.ps1
```

Installers are immutable. A published version can never be replaced, only
superseded, and clients ignore any manifest version older than the one they are
running — so a bad release is fixed by publishing a newer one, not by rolling
the manifest back.

### Diagnostics

Every session writes structured JSONL counters and timings to
`%LOCALAPPDATA%\orange\diagnostics`. Nothing is uploaded. When someone reports a
problem, ask them for those files: **Settings → Diagnostics → Open folder**.

Records contain counters, timings, and build identifiers — not media, tokens,
room codes, SDP, ICE, or window titles.

## Build

```powershell
winget install gstreamerproject.gstreamer Rustlang.Rustup pkgconf.pkgconf
winget install Microsoft.VisualStudio.2022.BuildTools --override "--quiet --wait --add Microsoft.VisualStudio.Workload.VCTools"

. .\dev.ps1
cargo build --locked
```

Install the pre-push hook once. It runs formatting, Clippy, and the media-free
crate tests in about fifteen seconds; bypass it with `git push --no-verify`:

```powershell
git config core.hooksPath packaging/hooks
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
connections and 16 viewers per room. Each peer has a 64-message outbound queue;
inbound signalling permits 256 text messages per 10 seconds, and each WebSocket
message is capped at 64 KiB.

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
There is no TURN configuration. Peers must establish a direct ICE path; networks
that require TURN are currently unsupported.

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

MIT. GPUI is Apache-2.0.

The installer redistributes part of the GStreamer runtime, dynamically linked
and unmodified: LGPL-2.1-or-later for GStreamer itself and most plugins,
MPL-2.0 for the AV1 RTP payloader from `gst-plugins-rs`, BSD-2-Clause for
`dav1d`, Apache-2.0 for OpenSSL. Upstream's licence texts ship alongside the
binaries in `gstreamer\share\licenses`, and sources are at
<https://gitlab.freedesktop.org/gstreamer/gstreamer>.
