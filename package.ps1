param(
    [switch]$SkipTests
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $MyInvocation.MyCommand.Path
$previousBuildId = $env:ORANGE_BUILD_ID
$previousChannel = $env:ORANGE_UPDATE_CHANNEL
$iscc = @(
    (Join-Path $env:LOCALAPPDATA "Programs\Inno Setup 6\ISCC.exe")
    (Join-Path ${env:ProgramFiles(x86)} "Inno Setup 6\ISCC.exe")
) | Where-Object { Test-Path $_ } | Select-Object -First 1

if (-not $iscc) {
    throw "Inno Setup 6 was not found. Install it with: winget install JRSoftware.InnoSetup"
}

Push-Location $root
try {
    . .\dev.ps1
    if (-not $env:GSTREAMER_1_0_ROOT_MSVC_X86_64) {
        throw "The GStreamer development environment is unavailable."
    }

    if (-not $SkipTests) {
        cargo test --workspace
        if ($LASTEXITCODE -ne 0) {
            throw "Workspace tests failed."
        }
    }

    $env:ORANGE_BUILD_ID = (git rev-parse HEAD).Trim()
    $env:ORANGE_UPDATE_CHANNEL = "beta"
    cargo build --locked --release -p orange -p orange-tray -p orange-updater
    if ($LASTEXITCODE -ne 0) {
        throw "Release build failed."
    }

    $redistDirectory = Join-Path $root "target\package"
    $redistTarget = Join-Path $redistDirectory "vc_redist.x64.exe"
    New-Item -ItemType Directory -Path $redistDirectory -Force | Out-Null
    $redistPattern = Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio\*\*\VC\Redist\MSVC\*\vc_redist.x64.exe"
    $redistSource = Get-ChildItem $redistPattern -ErrorAction SilentlyContinue |
        Sort-Object { $_.VersionInfo.FileVersionRaw } -Descending |
        Select-Object -First 1
    if ($redistSource) {
        Copy-Item $redistSource.FullName $redistTarget -Force
    } else {
        Invoke-WebRequest "https://aka.ms/vc14/vc_redist.x64.exe" -OutFile $redistTarget
    }
    $signature = Get-AuthenticodeSignature $redistTarget
    if ($signature.Status -ne "Valid" -or $signature.SignerCertificate.Subject -notlike "*Microsoft Corporation*") {
        throw "The Microsoft Visual C++ runtime signature is invalid."
    }

    $metadata = cargo metadata --no-deps --format-version 1 | ConvertFrom-Json
    $version = ($metadata.packages | Where-Object name -eq "orange-tray").version
    & $iscc "/DAppVersion=$version" ".\packaging\windows\orange.iss"
    if ($LASTEXITCODE -ne 0) {
        throw "Installer build failed."
    }

    Write-Host "Installer: $root\dist\orange-setup-$version.exe" -ForegroundColor Green
}
finally {
    $env:ORANGE_BUILD_ID = $previousBuildId
    $env:ORANGE_UPDATE_CHANNEL = $previousChannel
    Pop-Location
}
