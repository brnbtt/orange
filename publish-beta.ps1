[CmdletBinding()]
param(
    [switch]$Publish,
    [switch]$SkipTests,
    [switch]$LibraryOnly,
    [string]$Notes = "First beta: faster joining, automatic updates, and streamlined installation."
)

$ErrorActionPreference = "Stop"
$script:BetaStorageAccount = "orangealpha0d8d5893e69a3"
$script:BetaContainer = "releases"
$script:Repository = "brnbtt/orange"

function New-BetaManifest {
    param(
        [Parameter(Mandatory = $true)][string]$Version,
        [Parameter(Mandatory = $true)][string]$Build,
        [Parameter(Mandatory = $true)][string]$InstallerPath,
        [Parameter(Mandatory = $true)][string]$Notes
    )

    $installerName = Split-Path -Leaf $InstallerPath
    [pscustomobject][ordered]@{
        schema = 1
        channel = "beta"
        version = $Version
        build = $Build
        installer_url = "https://$script:BetaStorageAccount.blob.core.windows.net/$script:BetaContainer/$installerName"
        sha256 = (Get-FileHash -LiteralPath $InstallerPath -Algorithm SHA256).Hash.ToUpperInvariant()
        notes = $Notes
    }
}

function Test-BetaManifest {
    param(
        [Parameter(Mandatory = $true)]$Manifest,
        [Parameter(Mandatory = $true)][string]$InstallerPath
    )

    $expected = @("schema", "channel", "version", "build", "installer_url", "sha256", "notes")
    $actual = @($Manifest.PSObject.Properties | ForEach-Object { $_.Name })
    if ($actual.Count -ne $expected.Count -or @($expected | Where-Object { $actual -cnotcontains $_ }).Count) {
        throw "Invalid beta manifest fields"
    }
    if ($Manifest.schema -ne 1 -or $Manifest.channel -cne "beta") { throw "Invalid beta manifest channel" }
    if ($Manifest.version -cnotmatch '^[0-9]+\.[0-9]+\.[0-9]+-beta\.[0-9]+$') { throw "Invalid beta version" }
    if ($Manifest.build -cnotmatch '^[0-9a-f]{40}$') { throw "Invalid beta build" }
    if ($Manifest.sha256 -cnotmatch '^[0-9A-F]{64}$') { throw "Invalid installer hash" }
    if ($Manifest.notes -isnot [string] -or $Manifest.notes.Length -gt 500 -or
        @($Manifest.notes.ToCharArray() | Where-Object { [char]::IsControl($_) }).Count -ne 0) {
        throw "Invalid release notes"
    }
    $expectedName = "orange-setup-$($Manifest.version).exe"
    $expectedUrl = "https://$script:BetaStorageAccount.blob.core.windows.net/$script:BetaContainer/$expectedName"
    if ($Manifest.installer_url -cne $expectedUrl) { throw "Invalid installer URL" }
    if ((Split-Path -Leaf $InstallerPath) -cne $expectedName) { throw "Installer filename does not match version" }
    if ((Get-Item -LiteralPath $InstallerPath).Length -gt (250MB)) {
        throw "Installer exceeds the 250 MiB client limit"
    }
    if ((Get-FileHash -LiteralPath $InstallerPath -Algorithm SHA256).Hash -cne $Manifest.sha256) {
        throw "Installer checksum does not match manifest"
    }
    return $true
}

function Invoke-Checked {
    param([Parameter(Mandatory = $true)][string]$Command, [Parameter(Mandatory = $true)][string[]]$Arguments)
    & $Command @Arguments
    if ($LASTEXITCODE -ne 0) { throw "$Command failed with exit code $LASTEXITCODE" }
}

function Test-NativeSuccess {
    param([Parameter(Mandatory = $true)][string]$Command, [Parameter(Mandatory = $true)][string[]]$Arguments)
    $old = $ErrorActionPreference
    try {
        $ErrorActionPreference = "Continue"
        & $Command @Arguments *> $null
        return $LASTEXITCODE -eq 0
    } finally {
        $ErrorActionPreference = $old
    }
}

function Invoke-BetaPublish {
    $root = $PSScriptRoot
    Push-Location $root
    try {
        $build = (& git rev-parse HEAD).Trim()
        if ($LASTEXITCODE -ne 0 -or $build -cnotmatch '^[0-9a-f]{40}$') { throw "Could not determine build commit" }
        if ($Publish) {
            & git diff --quiet --ignore-submodules --
            $worktreeDirty = $LASTEXITCODE -ne 0
            & git diff --cached --quiet --ignore-submodules --
            $indexDirty = $LASTEXITCODE -ne 0
            if ($worktreeDirty -or $indexDirty) { throw "Publishing requires a clean tracked working tree" }
            $remoteBuild = (& git rev-parse origin/main).Trim()
            if ($LASTEXITCODE -ne 0 -or $remoteBuild -cne $build) {
                throw "Publishing requires HEAD to equal origin/main"
            }
        }

        & (Join-Path $root "packaging\windows\test-installer.ps1")
        & (Join-Path $root "packaging\windows\test-beta-publish.ps1")
        & (Join-Path $root "package.ps1") -SkipTests:$SkipTests
        if ($LASTEXITCODE -ne 0) { throw "Installer packaging failed" }
        $metadata = cargo metadata --no-deps --format-version 1 | ConvertFrom-Json
        $version = ($metadata.packages | Where-Object name -eq "orange-tray").version
        if ($version -cnotmatch '^[0-9]+\.[0-9]+\.[0-9]+-beta\.[0-9]+$') { throw "Workspace version is not a beta version" }
        $installerName = "orange-setup-$version.exe"
        $installer = Join-Path $root "dist\$installerName"
        if (-not (Test-Path -LiteralPath $installer -PathType Leaf)) { throw "Installer was not created" }

        $manifest = New-BetaManifest -Version $version -Build $build -InstallerPath $installer -Notes $Notes
        Test-BetaManifest -Manifest $manifest -InstallerPath $installer | Out-Null
        $manifestPath = Join-Path $root "dist\orange-beta.json"
        [IO.File]::WriteAllText(
            $manifestPath,
            ($manifest | ConvertTo-Json -Depth 4),
            (New-Object Text.UTF8Encoding($false)))

        if ($Publish) {
            Invoke-Checked -Command "az" -Arguments @(
                "storage", "blob", "upload", "--account-name", $script:BetaStorageAccount,
                "--container-name", $script:BetaContainer, "--file", $installer,
                "--name", $installerName, "--auth-mode", "key", "--overwrite", "false", "--only-show-errors"
            )

            $tag = "v$version"
            if (Test-NativeSuccess -Command "gh" -Arguments @("release", "view", $tag, "--repo", $script:Repository)) {
                throw "Release $tag already exists; beta versions are immutable"
            }
            Invoke-Checked -Command "gh" -Arguments @(
                "release", "create", $tag, $installer, $manifestPath,
                "--repo", $script:Repository, "--target", $build,
                "--title", "Orange $version", "--notes", $Notes, "--prerelease", "--latest=false"
            )
            Invoke-Checked -Command "az" -Arguments @(
                "storage", "blob", "upload", "--account-name", $script:BetaStorageAccount,
                "--container-name", $script:BetaContainer, "--file", $manifestPath,
                "--name", "orange-beta.json", "--auth-mode", "key", "--overwrite", "true", "--only-show-errors"
            )
        }

        Write-Host "Beta: $version ($build)"
        Write-Host "Installer: $installer"
        Write-Host "SHA-256: $($manifest.sha256)"
        Write-Host "Manifest: $manifestPath"
    } finally {
        Pop-Location
    }
}

if (-not $LibraryOnly) {
    Invoke-BetaPublish
}
