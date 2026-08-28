# orange

Low-overhead window streaming for friends. Share a game window at high bitrate
without the compression Discord puts on it, and without costing yourself FPS.

## Status

Milestone 1 of 6. Capture and encode work; there is no networking yet.

| # | Milestone | State |
| --- | --- | --- |
| 1 | Window enumeration, GPU capture, hardware encode | **done** |
| 2 | WebRTC transport, no transcode | **done** (loopback) |
| 3 | Signalling relay, host/watch as separate processes | **done** (one machine) |
| 4 | Two machines across the internet | next |
| 5 | Viewer window: borderless, rounded, overlay controls | |
| 6 | Game audio (`wasapi2src` + `opusenc`) | |
| 7 | Installer, tray, autostart | |

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

It knows nothing about media. It matches two peers by room code and forwards a
few kilobytes of SDP and ICE, then gets out of the way — video goes directly
peer to peer. That is what keeps hosting costs near zero, and why a tiny VM is
enough regardless of how many people are watching.

**The room code is the only credential.** Anyone you give it to can watch, and
can pass it on. That is the deliberate cost of "no accounts, no logins".

Public STUN (`stun.l.google.com:19302`) is used for address discovery. Peers
that cannot hole-punch will need a TURN server, which *does* relay video and
therefore costs real bandwidth.

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
