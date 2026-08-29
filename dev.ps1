# Sets up the environment for building orange.
#
#   . .\dev.ps1        (note the leading dot - it must run in your shell)
#
# GStreamer is not on PATH by default, and gstreamer-rs needs pkg-config to
# find its .pc files at build time.

$gst = @(
    $env:GSTREAMER_1_0_ROOT_MSVC_X86_64
    "$env:LOCALAPPDATA\Programs\gstreamer\1.0\msvc_x86_64"
    "C:\gstreamer\1.0\msvc_x86_64"
    "$env:ProgramFiles\gstreamer\1.0\msvc_x86_64"
) | Where-Object {
    $_ -and (Test-Path (Join-Path $_ "bin\gstreamer-1.0-0.dll"))
} | Select-Object -First 1

if (-not (Test-Path $gst)) {
    Write-Error "GStreamer not found at $gst. Install with: winget install gstreamerproject.gstreamer"
    return
}

$env:GSTREAMER_1_0_ROOT_MSVC_X86_64 = "$gst\"
$env:PKG_CONFIG_PATH = "$gst\lib\pkgconfig"
$env:Path = "$gst\bin;$env:USERPROFILE\.cargo\bin;$env:Path"

Write-Host "GStreamer  $gst" -ForegroundColor DarkGray
Write-Host "ready - try: cargo run -- list" -ForegroundColor Green
