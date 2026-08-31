param(
    [switch]$SkipTests,
    [string]$BuildId
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $MyInvocation.MyCommand.Path
$previousBuildId = $env:ORANGE_BUILD_ID
$previousChannel = $env:ORANGE_UPDATE_CHANNEL
$previousLinker = $env:CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER
$packageLock = $null
$iscc = @(
    (Join-Path $env:LOCALAPPDATA "Programs\Inno Setup 6\ISCC.exe")
    (Join-Path ${env:ProgramFiles(x86)} "Inno Setup 6\ISCC.exe")
) | Where-Object { Test-Path $_ } | Select-Object -First 1

if (-not $iscc) {
    throw "Inno Setup 6 was not found. Install it with: winget install JRSoftware.InnoSetup"
}

Push-Location $root
try {
    $targetDirectory = Join-Path $root "target"
    New-Item -ItemType Directory -Path $targetDirectory -Force | Out-Null
    try {
        $packageLock = [IO.File]::Open(
            (Join-Path $targetDirectory "orange-package.lock"),
            [IO.FileMode]::OpenOrCreate,
            [IO.FileAccess]::ReadWrite,
            [IO.FileShare]::None)
    } catch {
        throw "Another Orange packaging process is already running."
    }
    . .\dev.ps1
    . .\packaging\windows\package-provenance.ps1
    if (-not $env:GSTREAMER_1_0_ROOT_MSVC_X86_64) {
        throw "The GStreamer development environment is unavailable."
    }

    $BuildId = Assert-PackageProvenance -Root $root -BuildId $BuildId
    if (-not $SkipTests) {
        cargo test --locked --workspace
        if ($LASTEXITCODE -ne 0) {
            throw "Workspace tests failed."
        }
    }

    $env:ORANGE_BUILD_ID = $BuildId
    $env:ORANGE_UPDATE_CHANNEL = "beta"
    # .cargo/config.toml selects rust-lld to keep local iteration fast. Release
    # artifacts stay on link.exe, which is what every shipped beta used.
    $env:CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER = "link.exe"
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
    Assert-PackageProvenance -Root $root -BuildId $BuildId | Out-Null
    & $iscc "/DAppVersion=$version" ".\packaging\windows\orange.iss"
    if ($LASTEXITCODE -ne 0) {
        throw "Installer build failed."
    }

    Write-Host "Installer: $root\dist\orange-setup-$version.exe" -ForegroundColor Green
}
finally {
    if ($packageLock) { $packageLock.Dispose() }
    $env:ORANGE_BUILD_ID = $previousBuildId
    $env:ORANGE_UPDATE_CHANNEL = $previousChannel
    $env:CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER = $previousLinker
    Pop-Location
}
