# Sets up the environment for building orange.
#
#   . .\dev.ps1        (note the leading dot - it must run in your shell)
#
# GStreamer is not on PATH by default, and gstreamer-rs needs pkg-config to
# find its .pc files at build time.

$gst = "$env:LOCALAPPDATA\Programs\gstreamer\1.0\msvc_x86_64"

if (-not (Test-Path $gst)) {
    Write-Error "GStreamer not found at $gst. Install with: winget install gstreamerproject.gstreamer"
    return
}

$env:GSTREAMER_1_0_ROOT_MSVC_X86_64 = "$gst\"
$env:PKG_CONFIG_PATH = "$gst\lib\pkgconfig"
$env:Path = "$gst\bin;$env:USERPROFILE\.cargo\bin;$env:Path"

Write-Host "GStreamer  $gst" -ForegroundColor DarkGray
Write-Host "ready - try: cargo run -- list" -ForegroundColor Green
