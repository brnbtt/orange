[CmdletBinding()]
param(
    [switch]$Publish,
    [switch]$SkipTests,
    [switch]$LibraryOnly,
    [string]$Notes = ""
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
    if ($Manifest.notes -isnot [string] -or [Text.Encoding]::UTF8.GetByteCount($Manifest.notes) -gt 500 -or
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
    # Native tools write ordinary progress to stderr (git fetch, az, gh). Under
    # ErrorActionPreference=Stop that becomes a terminating NativeCommandError
    # before the exit code is ever read, so success must be judged by the code.
    $previous = $ErrorActionPreference
    try {
        $ErrorActionPreference = "Continue"
        & $Command @Arguments 2>&1 | ForEach-Object { Write-Host "   $_" -ForegroundColor DarkGray }
    } finally { $ErrorActionPreference = $previous }
    if ($LASTEXITCODE -ne 0) { throw "$Command failed with exit code $LASTEXITCODE" }
}

function Get-OptionalNativeJson {
    param([Parameter(Mandatory = $true)][string]$Command, [Parameter(Mandatory = $true)][string[]]$Arguments)
    $old = $ErrorActionPreference
    try {
        $ErrorActionPreference = "Continue"
        $output = & $Command @Arguments 2>$null
        if ($LASTEXITCODE -ne 0) { return $null }
        return ($output | Out-String | ConvertFrom-Json)
    } finally {
        $ErrorActionPreference = $old
    }
}

function Get-WorkspaceVersion {
    $metadata = cargo metadata --no-deps --format-version 1 | ConvertFrom-Json
    if ($LASTEXITCODE -ne 0) { throw "Could not read the workspace version" }
    ($metadata.packages | Where-Object name -eq "orange-tray").version
}

# Fails in seconds on the two mistakes that otherwise surface only after a full
# release build: forgetting to bump the version, and forgetting -Notes.
function Assert-Publishable {
    param(
        [Parameter(Mandatory = $true)][string]$Version,
        [Parameter(Mandatory = $true)][string]$Build,
        [Parameter(Mandatory = $true)][AllowEmptyString()][string]$Notes
    )

    if ($Version -cnotmatch '^[0-9]+\.[0-9]+\.[0-9]+-beta\.[0-9]+$') {
        throw "Workspace version '$Version' is not a beta version. Set it in Cargo.toml."
    }
    if ([string]::IsNullOrWhiteSpace($Notes)) {
        throw "Publishing requires -Notes. Users see this text in the update banner."
    }
    if ([Text.Encoding]::UTF8.GetByteCount($Notes) -gt 500) {
        throw "Release notes exceed the 500 byte client limit."
    }

    $existing = Get-OptionalNativeJson -Command "az" -Arguments @(
        "storage", "blob", "show", "--account-name", $script:BetaStorageAccount,
        "--container-name", $script:BetaContainer, "--name", "orange-setup-$Version.exe",
        "--auth-mode", "key", "--output", "json", "--only-show-errors"
    )
    if ($null -ne $existing -and $existing.metadata.build -cne $Build) {
        throw ("Version $Version was already published from commit $($existing.metadata.build). " +
               "Installers are immutable: bump the version in Cargo.toml.")
    }
}

function Ensure-AzureInstaller {
    param(
        [Parameter(Mandatory = $true)][string]$Installer,
        [Parameter(Mandatory = $true)][string]$InstallerName,
        [Parameter(Mandatory = $true)][string]$Build,
        [Parameter(Mandatory = $true)][string]$Sha256
    )
    $existing = Get-OptionalNativeJson -Command "az" -Arguments @(
        "storage", "blob", "show", "--account-name", $script:BetaStorageAccount,
        "--container-name", $script:BetaContainer, "--name", $InstallerName,
        "--auth-mode", "key", "--output", "json", "--only-show-errors"
    )
    if ($null -eq $existing) {
        Invoke-Checked -Command "az" -Arguments @(
            "storage", "blob", "upload", "--account-name", $script:BetaStorageAccount,
            "--container-name", $script:BetaContainer, "--file", $Installer,
            "--name", $InstallerName, "--auth-mode", "key", "--overwrite", "false",
            "--metadata", "sha256=$Sha256", "build=$Build", "--only-show-errors"
        )
    } else {
        $size = (Get-Item -LiteralPath $Installer).Length
        if ([int64]$existing.properties.contentLength -ne $size -or
            $existing.metadata.sha256 -cne $Sha256 -or $existing.metadata.build -cne $Build) {
            throw "Existing Azure beta installer does not match this immutable build"
        }
    }
    $check = Join-Path $env:TEMP ("orange-azure-check-" + [guid]::NewGuid().ToString("N") + ".exe")
    try {
        Invoke-Checked -Command "az" -Arguments @(
            "storage", "blob", "download", "--account-name", $script:BetaStorageAccount,
            "--container-name", $script:BetaContainer, "--name", $InstallerName,
            "--file", $check, "--auth-mode", "key", "--overwrite", "true", "--only-show-errors"
        )
        if ((Get-FileHash -LiteralPath $check -Algorithm SHA256).Hash -cne $Sha256) {
            throw "Azure beta installer failed remote SHA-256 verification"
        }
    } finally {
        Remove-Item -LiteralPath $check -Force -ErrorAction SilentlyContinue
    }
}

function Resolve-GitHubTagCommit {
    param([Parameter(Mandatory = $true)][string]$Tag)
    $reference = Get-OptionalNativeJson -Command "gh" -Arguments @(
        "api", "repos/$script:Repository/git/ref/tags/$Tag"
    )
    if ($null -eq $reference) { return $null }
    $object = $reference.object
    for ($depth = 0; $depth -lt 5 -and $object.type -eq "tag"; $depth++) {
        $tagObject = Get-OptionalNativeJson -Command "gh" -Arguments @(
            "api", "repos/$script:Repository/git/tags/$($object.sha)"
        )
        if ($null -eq $tagObject) { throw "Could not peel GitHub beta tag" }
        $object = $tagObject.object
    }
    if ($object.type -ne "commit" -or $object.sha -cnotmatch '^[0-9a-f]{40}$') {
        throw "GitHub beta tag does not resolve to a commit"
    }
    return [string]$object.sha
}

function Ensure-GitHubRelease {
    param(
        [Parameter(Mandatory = $true)][string]$Tag,
        [Parameter(Mandatory = $true)][string]$Build,
        [Parameter(Mandatory = $true)][string]$Title,
        [Parameter(Mandatory = $true)][string]$Notes,
        [Parameter(Mandatory = $true)][string[]]$Assets
    )
    $release = Get-OptionalNativeJson -Command "gh" -Arguments @(
        "release", "view", $Tag, "--repo", $script:Repository,
        "--json", "targetCommitish,assets"
    )
    $tagCommit = Resolve-GitHubTagCommit -Tag $Tag
    if ($null -ne $tagCommit -and $tagCommit -cne $Build) {
        throw "Existing GitHub beta tag targets a different build"
    }
    if ($null -eq $release) {
        Invoke-Checked -Command "gh" -Arguments @(
            "release", "create", $Tag, "--repo", $script:Repository, "--target", $Build,
            "--title", $Title, "--notes", $Notes, "--prerelease", "--latest=false"
        )
        $release = [pscustomobject]@{ targetCommitish = $Build; assets = @() }
    }
    $tagCommit = Resolve-GitHubTagCommit -Tag $Tag
    if ($tagCommit -cne $Build) {
        throw "Existing GitHub beta tag targets a different build"
    }
    foreach ($asset in $Assets) {
        $name = Split-Path -Leaf $asset
        $remote = @($release.assets | Where-Object { $_.name -ceq $name })
        if ($remote.Count -eq 0) {
            Invoke-Checked -Command "gh" -Arguments @(
                "release", "upload", $Tag, $asset, "--repo", $script:Repository
            )
        } elseif ($remote.Count -ne 1 -or [int64]$remote[0].size -ne (Get-Item -LiteralPath $asset).Length) {
            throw "Existing GitHub asset $name does not match this build"
        }
        $check = Join-Path $env:TEMP ("orange-beta-check-" + [guid]::NewGuid().ToString("N"))
        New-Item -ItemType Directory -Path $check | Out-Null
        try {
            Invoke-Checked -Command "gh" -Arguments @(
                "release", "download", $Tag, "--repo", $script:Repository,
                "--pattern", $name, "--dir", $check
            )
            if ((Get-FileHash -LiteralPath (Join-Path $check $name) -Algorithm SHA256).Hash -cne
                (Get-FileHash -LiteralPath $asset -Algorithm SHA256).Hash) {
                throw "GitHub asset $name failed remote SHA-256 verification"
            }
        } finally {
            Remove-Item -LiteralPath $check -Recurse -Force -ErrorAction SilentlyContinue
        }
    }
}

function Invoke-BetaPublish {
    $root = $PSScriptRoot
    $publishStage = $null
    Push-Location $root
    try {
        if ($Publish) {
            Invoke-Checked -Command "git" -Arguments @("fetch", "origin", "main")
        }
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
        $version = Get-WorkspaceVersion
        if ($Publish) {
            Assert-Publishable -Version $version -Build $build -Notes $Notes
        } elseif ($version -cnotmatch '^[0-9]+\.[0-9]+\.[0-9]+-beta\.[0-9]+$') {
            throw "Workspace version '$version' is not a beta version. Set it in Cargo.toml."
        }
        Write-Host "==> Publishing $version from $($build.Substring(0, 12))" -ForegroundColor Cyan

        & (Join-Path $root "package.ps1") -SkipTests:$SkipTests -BuildId $build
        if ($LASTEXITCODE -ne 0) { throw "Installer packaging failed" }
        $installerName = "orange-setup-$version.exe"
        $outputInstaller = Join-Path $root "dist\$installerName"
        if (-not (Test-Path -LiteralPath $outputInstaller -PathType Leaf)) { throw "Installer was not created" }
        $installer = $outputInstaller
        if ($Publish) {
            $publishStage = Join-Path $root ("dist\.publish-" + [guid]::NewGuid().ToString("N"))
            New-Item -ItemType Directory -Path $publishStage | Out-Null
            $installer = Join-Path $publishStage $installerName
            Copy-Item -LiteralPath $outputInstaller -Destination $installer
        }

        $manifest = New-BetaManifest -Version $version -Build $build -InstallerPath $installer -Notes $Notes
        Test-BetaManifest -Manifest $manifest -InstallerPath $installer | Out-Null
        $outputManifest = Join-Path $root "dist\orange-beta.json"
        $manifestPath = if ($Publish) {
            Join-Path $publishStage "orange-beta.json"
        } else {
            $outputManifest
        }
        [IO.File]::WriteAllText(
            $manifestPath,
            ($manifest | ConvertTo-Json -Depth 4),
            (New-Object Text.UTF8Encoding($false)))

        if ($Publish) {
            $afterBuild = (& git rev-parse HEAD).Trim()
            & git diff --quiet --ignore-submodules --
            $dirtyAfterBuild = $LASTEXITCODE -ne 0
            & git diff --cached --quiet --ignore-submodules --
            $dirtyIndexAfterBuild = $LASTEXITCODE -ne 0
            if ($afterBuild -cne $build -or $dirtyAfterBuild -or $dirtyIndexAfterBuild) {
                throw "Source changed while building the beta installer"
            }
            Ensure-AzureInstaller -Installer $installer -InstallerName $installerName -Build $build -Sha256 $manifest.sha256
            $tag = "v$version"
            Ensure-GitHubRelease -Tag $tag -Build $build -Title "Orange $version" -Notes $Notes -Assets @($installer, $manifestPath)
            Invoke-Checked -Command "az" -Arguments @(
                "storage", "blob", "upload", "--account-name", $script:BetaStorageAccount,
                "--container-name", $script:BetaContainer, "--file", $manifestPath,
                "--name", "orange-beta.json", "--auth-mode", "key", "--overwrite", "true", "--only-show-errors"
            )
            Copy-Item -LiteralPath $manifestPath -Destination $outputManifest -Force
        }

        Write-Host "Beta: $version ($build)"
        Write-Host "Installer: $outputInstaller"
        Write-Host "SHA-256: $($manifest.sha256)"
        Write-Host "Manifest: $outputManifest"
    } finally {
        if ($publishStage) {
            Remove-Item -LiteralPath $publishStage -Recurse -Force -ErrorAction SilentlyContinue
        }
        Pop-Location
    }
}

if (-not $LibraryOnly) {
    Invoke-BetaPublish
}
