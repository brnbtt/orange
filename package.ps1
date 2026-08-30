param(
    [switch]$SkipTests,
    [string]$BuildId
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $MyInvocation.MyCommand.Path
$previousBuildId = $env:ORANGE_BUILD_ID
$previousChannel = $env:ORANGE_UPDATE_CHANNEL
$previousManifestUrl = $env:ORANGE_UPDATE_MANIFEST_URL
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

    $headBuild = (git rev-parse HEAD).Trim()
    if ($BuildId) {
        if ($BuildId -cnotmatch '^[0-9a-f]{40}$' -or $BuildId -cne $headBuild) {
            throw "BuildId must match the current committed HEAD."
        }
    } else {
        $BuildId = $headBuild
    }
    $env:ORANGE_BUILD_ID = $BuildId
    $env:ORANGE_UPDATE_CHANNEL = "beta"
    Remove-Item Env:ORANGE_UPDATE_MANIFEST_URL -ErrorAction SilentlyContinue
    cargo build --locked --release -p orange -p orange-tray -p orange-updater
    if ($LASTEXITCODE -ne 0) {
        throw "Release build failed."
    }

    $runtimeDirectory = Join-Path $root "target\package"
    $runtimeTarget = Join-Path $runtimeDirectory "vcruntime140.dll"
    New-Item -ItemType Directory -Path $runtimeDirectory -Force | Out-Null
    $runtimePatterns = @(
        (Join-Path $env:ProgramFiles "Microsoft Visual Studio\*\*\VC\Redist\MSVC\*\x64\Microsoft.VC*.CRT\vcruntime140.dll")
        (Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio\*\*\VC\Redist\MSVC\*\x64\Microsoft.VC*.CRT\vcruntime140.dll")
    )
    $runtimeSource = Get-ChildItem $runtimePatterns -ErrorAction SilentlyContinue |
        Sort-Object { $_.VersionInfo.FileVersionRaw } -Descending |
        Select-Object -First 1
    if (-not $runtimeSource) {
        throw "vcruntime140.dll was not found in the Visual Studio redistributable directories."
    }
    Copy-Item $runtimeSource.FullName $runtimeTarget -Force
    $signature = Get-AuthenticodeSignature $runtimeTarget
    if ($signature.Status -ne "Valid" -or $signature.SignerCertificate.Subject -notlike "*Microsoft Corporation*") {
        throw "The Microsoft Visual C++ runtime DLL signature is invalid."
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
    $env:ORANGE_UPDATE_MANIFEST_URL = $previousManifestUrl
    Pop-Location
}
