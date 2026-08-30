[CmdletBinding()]
param(
    [switch]$Publish,
    [switch]$SkipTests,
    [switch]$LibraryOnly,
    [string]$Profile = "hardware-bounded-jitter",
    [string]$Av1Decoder = "hardware",
    [string]$RtpBufferMode = ""
)

$ErrorActionPreference = "Stop"
$script:Repository = "brnbtt/orange"
$script:ReleaseTag = "alpha-latest"

function New-AlphaManifest {
    param(
        [Parameter(Mandatory = $true)][string]$Build,
        [Parameter(Mandatory = $true)][string]$AssetName,
        [Parameter(Mandatory = $true)][string]$Sha256,
        [Parameter(Mandatory = $true)][string]$Profile,
        [Parameter(Mandatory = $true)]$Environment
    )

    return [pscustomobject][ordered]@{
        schema = 1
        build = $Build
        asset_url = "https://github.com/$script:Repository/releases/download/$script:ReleaseTag/$AssetName"
        sha256 = $Sha256.ToUpperInvariant()
        profile = $Profile
        environment = [pscustomobject]$Environment
    }
}

function Test-AlphaPublishManifest {
    param([Parameter(Mandatory = $true)]$Manifest)

    $launcher = Join-Path $PSScriptRoot "orange-alpha.ps1"
    if (-not (Test-Path -LiteralPath $launcher -PathType Leaf)) {
        throw "Alpha launcher is missing"
    }
    . $launcher
    $json = $Manifest | ConvertTo-Json -Depth 8
    $validated = ConvertTo-AlphaManifest -Json $json
    if ($validated.asset_url -cnotmatch "^https://github\.com/brnbtt/orange/releases/download/alpha-latest/orange-alpha-[0-9a-f]{7}\.zip$") {
        throw "Invalid published asset URL"
    }
    return $validated
}

function Test-AlphaPublishArchive {
    param([Parameter(Mandatory = $true)][string]$Path)

    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        throw "Alpha archive is missing"
    }
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $archive = [IO.Compression.ZipFile]::OpenRead($Path)
    try {
        $files = @($archive.Entries | Where-Object { -not [string]::IsNullOrEmpty($_.Name) } | ForEach-Object { $_.FullName.Replace("\", "/") })
        $expected = @("README.txt", "orange-tray.exe", "orange.exe")
        if ($files.Count -ne $expected.Count -or @($expected | Where-Object { $files -cnotcontains $_ }).Count -ne 0) {
            throw "Alpha archive must contain exactly orange.exe, orange-tray.exe, and README.txt"
        }
    } finally {
        $archive.Dispose()
    }
    return $true
}

function Invoke-CheckedCommand {
    param(
        [Parameter(Mandatory = $true)][string]$Command,
        [Parameter(Mandatory = $true)][string[]]$Arguments
    )

    & $Command @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "$Command failed with exit code $LASTEXITCODE"
    }
}

function Test-NativeCommandSucceeds {
    param(
        [Parameter(Mandatory = $true)][string]$Command,
        [Parameter(Mandatory = $true)][string[]]$Arguments
    )

    $previousPreference = $ErrorActionPreference
    try {
        $ErrorActionPreference = "Continue"
        & $Command @Arguments *> $null
        return $LASTEXITCODE -eq 0
    } finally {
        $ErrorActionPreference = $previousPreference
    }
}

function Invoke-AlphaPublish {
    $root = Split-Path $PSScriptRoot -Parent
    Push-Location $root
    try {
        $build = (& git rev-parse HEAD).Trim()
        if ($LASTEXITCODE -ne 0 -or $build -cnotmatch "^[0-9a-f]{40}$") {
            throw "Could not determine a committed Git build ID"
        }
        $short = $build.Substring(0, 7)

        if ($Publish) {
            & git diff --quiet --ignore-submodules --
            $workingDirty = $LASTEXITCODE -ne 0
            & git diff --cached --quiet --ignore-submodules --
            $indexDirty = $LASTEXITCODE -ne 0
            if ($workingDirty -or $indexDirty) {
                throw "Publishing requires a clean tracked working tree"
            }
            & git merge-base --is-ancestor HEAD origin/main
            if ($LASTEXITCODE -ne 0) {
                throw "Publishing requires HEAD to exist on origin/main"
            }
        }

        if (-not $SkipTests) {
            . (Join-Path $root "dev.ps1")
            if (-not $env:GSTREAMER_1_0_ROOT_MSVC_X86_64) {
                throw "The GStreamer development environment is unavailable"
            }
            Invoke-CheckedCommand -Command "cargo" -Arguments @("test", "--workspace")
        } else {
            . (Join-Path $root "dev.ps1")
        }
        Invoke-CheckedCommand -Command "cargo" -Arguments @("build", "--locked", "--release", "-p", "orange", "-p", "orange-tray")

        $output = Join-Path $root "dist\alpha"
        $stage = Join-Path $output "stage-$short"
        New-Item -ItemType Directory -Path $output -Force | Out-Null
        Remove-Item -LiteralPath $stage -Recurse -Force -ErrorAction SilentlyContinue
        New-Item -ItemType Directory -Path $stage | Out-Null
        Copy-Item -LiteralPath (Join-Path $root "target\release\orange.exe") -Destination $stage
        Copy-Item -LiteralPath (Join-Path $root "target\release\orange-tray.exe") -Destination $stage
        @"
Orange Alpha $short

Launch this build through orange-alpha.cmd. The stable launcher verifies the
archive checksum, records diagnostics, and uploads them after Orange exits.
This portable build requires the GStreamer MSVC x64 runtime.
"@ | Set-Content -LiteralPath (Join-Path $stage "README.txt") -Encoding UTF8

        $assetName = "orange-alpha-$short.zip"
        $archive = Join-Path $output $assetName
        Remove-Item -LiteralPath $archive -Force -ErrorAction SilentlyContinue
        Compress-Archive -Path (Join-Path $stage "*") -DestinationPath $archive -CompressionLevel Optimal
        Test-AlphaPublishArchive -Path $archive | Out-Null
        $hash = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToUpperInvariant()
        $manifest = New-AlphaManifest -Build $build -AssetName $assetName -Sha256 $hash -Profile $Profile -Environment ([ordered]@{
            ORANGE_AV1_DECODER = $Av1Decoder
            ORANGE_RTP_BUFFER_MODE = $RtpBufferMode
        })
        Test-AlphaPublishManifest -Manifest $manifest | Out-Null
        $manifestPath = Join-Path $output "orange-alpha.json"
        $manifest | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $manifestPath -Encoding UTF8

        $launcherStage = Join-Path $output "launcher"
        $launcherArchive = Join-Path $output "orange-alpha-launcher.zip"
        Remove-Item -LiteralPath $launcherStage -Recurse -Force -ErrorAction SilentlyContinue
        Remove-Item -LiteralPath $launcherArchive -Force -ErrorAction SilentlyContinue
        New-Item -ItemType Directory -Path $launcherStage | Out-Null
        Copy-Item -LiteralPath (Join-Path $PSScriptRoot "orange-alpha.cmd") -Destination $launcherStage
        Copy-Item -LiteralPath (Join-Path $PSScriptRoot "orange-alpha.ps1") -Destination $launcherStage
        "Download once, extract, and run orange-alpha.cmd for every alpha test." |
            Set-Content -LiteralPath (Join-Path $launcherStage "README.txt") -Encoding UTF8
        Compress-Archive -Path (Join-Path $launcherStage "*") -DestinationPath $launcherArchive -CompressionLevel Optimal

        if ($Publish) {
            if ($null -eq (Get-Command gh -ErrorAction SilentlyContinue)) {
                throw "GitHub CLI is required for -Publish"
            }
            $releaseExists = Test-NativeCommandSucceeds -Command "gh" -Arguments @(
                "release", "view", $script:ReleaseTag, "--repo", $script:Repository
            )
            if (-not $releaseExists) {
                Invoke-CheckedCommand -Command "gh" -Arguments @(
                    "release", "create", $script:ReleaseTag,
                    "--repo", $script:Repository,
                    "--target", $build,
                    "--title", "Orange alpha channel",
                    "--notes", "Automatically updated diagnostic builds. Not a production installer.",
                    "--prerelease", "--latest=false"
                )
            }
            Invoke-CheckedCommand -Command "gh" -Arguments @("release", "upload", $script:ReleaseTag, $archive, "--repo", $script:Repository, "--clobber")
            Invoke-CheckedCommand -Command "gh" -Arguments @("release", "upload", $script:ReleaseTag, $manifestPath, "--repo", $script:Repository, "--clobber")
            Invoke-CheckedCommand -Command "gh" -Arguments @("release", "upload", $script:ReleaseTag, $launcherArchive, "--repo", $script:Repository, "--clobber")
        }

        Write-Host "Alpha build: $build"
        Write-Host "Archive: $archive"
        Write-Host "SHA-256: $hash"
        Write-Host "Manifest: $manifestPath"
        Write-Host "Launcher: $launcherArchive"
    } finally {
        Pop-Location
    }
}

if (-not $LibraryOnly) {
    Invoke-AlphaPublish
}
