# Orange Beta Installer And Updates Specification

## Goal

Ship `0.2.0-beta.1` as a one-click per-user Windows installer. Installed builds
check the public beta channel, show a native in-app update banner, and install a
newer verified installer after one user click before reopening Orange.

## Installer Experience

- Double-clicking the installer immediately shows installation progress; there
  are no welcome, destination, Start Menu, task-selection, ready, or completion
  pages.
- Installation remains per-user and does not require elevation for Orange.
- The current Microsoft VC++ redistributable is run silently and idempotently;
  GStreamer is installed silently only when missing.
- Orange opens automatically after a successful interactive installation.
- Silent update installation does not launch Orange itself; the detached
  updater reopens it after checking the installer exit status.
- The Start Menu shortcut and uninstaller remain available. Desktop and startup
  shortcuts are not created by the installer.

## Update Channel

Manifest URL:

`https://orangealpha0d8d5893e69a3.blob.core.windows.net/releases/orange-beta.json`

The JSON object has exactly these fields:

```json
{
  "schema": 1,
  "channel": "beta",
  "version": "0.2.0-beta.2",
  "build": "40 lowercase hexadecimal characters",
  "installer_url": "https://orangealpha0d8d5893e69a3.blob.core.windows.net/releases/orange-setup-0.2.0-beta.2.exe",
  "sha256": "64 hexadecimal characters",
  "notes": "Short release summary"
}
```

- The tray checks once at startup and then at most every six hours.
- Only a semantically newer version is offered; downgrades and the current
  version are ignored.
- The installer URL must use HTTPS, the exact Orange Azure host, the
  `/releases/` container, and the expected filename for the manifest version.
- Downloads are limited to 250 MiB, written to a temporary sibling file, and
  activated only after SHA-256 verification.
- Network, parse, and download failures are nonfatal and never interrupt
  hosting or viewing.

## Update Application

- The update banner is visible on every app screen without covering controls.
- Available state shows the target version and an `Update now` button.
- Downloading state provides visible progress/status and disables duplicate
  update requests.
- Failure state shows a concise reason and a `Retry` button.
- One click downloads the installer, verifies it, copies the updater to a
  temporary path, launches it, stops active media children, and quits the tray.
- The updater waits for the old tray PID, verifies the installer again, runs it
  silently, and reopens the installed tray only after a successful exit.
- On installer failure, the existing installation remains usable and the
  updater writes a bounded local error file without secrets.

## Trust And Safety

- This first beta is unsigned because no Authenticode certificate is available.
- HTTPS host pinning plus SHA-256 protects against accidental corruption but is
  not a substitute for signed release metadata and Authenticode. Production
  promotion is blocked until signing is added.
- Installer URLs, hashes, paths, and versions are strictly validated.
- Update processes never receive Discord tokens or diagnostics credentials.
- Update checks contain no user identity or telemetry.

## Publishing

- `package.ps1` produces `dist/orange-setup-0.2.0-beta.1.exe` and embeds the
  current Git commit into the tray build.
- `publish-beta.ps1` tests, packages, hashes, validates, uploads the installer,
  and uploads the manifest last so clients never observe an unpublished asset.
- Publishing is explicit through `-Publish`; local validation has no cloud side
  effects.
