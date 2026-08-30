[CmdletBinding()]
param(
    [string]$Root = (Join-Path $env:LOCALAPPDATA "Orange Alpha"),
    [string]$ManifestPath,
    [switch]$AllowLocalAssets
)

$ErrorActionPreference = "Stop"

$script:AlphaManifestUrl = "https://github.com/brnbtt/orange/releases/download/alpha-latest/orange-alpha.json"
$script:AlphaDefaultServer = "wss://orange-relay.redmushroom-80c79f12.brazilsouth.azurecontainerapps.io/ws"
$script:AlphaReservedEnvironment = @(
    "ORANGE_BUILD_ID",
    "ORANGE_RUN_ID",
    "ORANGE_DEVICE_ID",
    "ORANGE_TEST_PROFILE",
    "ORANGE_MEDIA_DIAGNOSTICS"
)

function Initialize-AlphaRoot {
    param([Parameter(Mandatory = $true)][string]$Root)

    New-Item -ItemType Directory -Path $Root -Force | Out-Null
    foreach ($name in @("versions", "diagnostics", "pending", "sent")) {
        New-Item -ItemType Directory -Path (Join-Path $Root $name) -Force | Out-Null
    }
}

function ConvertTo-AlphaManifest {
    param(
        [Parameter(Mandatory = $true)][string]$Json,
        [switch]$AllowLocalAssets
    )

    try {
        $manifest = $Json | ConvertFrom-Json
    } catch {
        throw "Invalid manifest JSON: $($_.Exception.Message)"
    }

    if ($null -eq $manifest -or $manifest -is [System.Array]) {
        throw "Invalid manifest: expected an object"
    }

    $expected = @("schema", "build", "asset_url", "sha256", "profile", "environment")
    $actual = @($manifest.PSObject.Properties | ForEach-Object { $_.Name })
    if ($actual.Count -ne $expected.Count -or @($expected | Where-Object { $actual -notcontains $_ }).Count -ne 0) {
        throw "Invalid manifest schema: expected exactly schema, build, asset_url, sha256, profile, and environment"
    }
    if ($manifest.schema -isnot [int] -or $manifest.schema -ne 1) {
        throw "Unsupported manifest schema"
    }
    if ($manifest.build -isnot [string] -or $manifest.build -cnotmatch "^[0-9a-f]{40}$") {
        throw "Invalid manifest build"
    }
    if ($manifest.sha256 -isnot [string] -or $manifest.sha256 -notmatch "^[0-9A-Fa-f]{64}$") {
        throw "Invalid manifest sha256"
    }
    $manifest.sha256 = $manifest.sha256.ToUpperInvariant()
    if ($manifest.profile -isnot [string] -or $manifest.profile -cnotmatch "^[a-z0-9-]{1,64}$" -or [Text.Encoding]::UTF8.GetByteCount($manifest.profile) -gt 64) {
        throw "Invalid manifest profile"
    }
    if ($manifest.asset_url -isnot [string] -or [string]::IsNullOrWhiteSpace($manifest.asset_url)) {
        throw "Invalid manifest asset URL"
    }

    $localAsset = Test-Path -LiteralPath $manifest.asset_url -PathType Leaf
    if ($localAsset) {
        if (-not $AllowLocalAssets) {
            throw "Local manifest assets require the explicit test parameter"
        }
        $manifest.asset_url = (Resolve-Path -LiteralPath $manifest.asset_url).Path
    } else {
        $assetUri = $null
        if (-not [Uri]::TryCreate($manifest.asset_url, [UriKind]::Absolute, [ref]$assetUri) -or $assetUri.Scheme -cne "https") {
            throw "Production manifest asset URLs must use HTTPS"
        }
    }

    if ($manifest.environment -isnot [pscustomobject]) {
        throw "Invalid manifest environment"
    }
    foreach ($property in @($manifest.environment.PSObject.Properties)) {
        if ($property.Name -cnotmatch "^ORANGE_[A-Z0-9_]+$") {
            throw "Invalid manifest environment key '$($property.Name)'"
        }
        if ($script:AlphaReservedEnvironment -contains $property.Name) {
            throw "Manifest environment key '$($property.Name)' is reserved"
        }
        if ($property.Value -isnot [string]) {
            throw "Manifest environment value '$($property.Name)' must be a string"
        }
    }

    return $manifest
}

function Read-AlphaManifest {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [switch]$AllowLocalAssets
    )

    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        throw "Manifest fixture does not exist: $Path"
    }
    return ConvertTo-AlphaManifest -Json (Get-Content -LiteralPath $Path -Raw) -AllowLocalAssets:$AllowLocalAssets
}

function Get-AlphaManifest {
    param(
        [string]$ManifestPath,
        [switch]$AllowLocalAssets
    )

    if (-not [string]::IsNullOrWhiteSpace($ManifestPath)) {
        return Read-AlphaManifest -Path $ManifestPath -AllowLocalAssets:$AllowLocalAssets
    }
    $response = Invoke-WebRequest -Uri $script:AlphaManifestUrl -UseBasicParsing
    return ConvertTo-AlphaManifest -Json $response.Content
}

function Write-AlphaJsonAtomically {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)]$Value
    )

    $temporary = "$Path.$([guid]::NewGuid().ToString('N')).tmp"
    try {
        $Value | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $temporary -Encoding UTF8
        Move-Item -LiteralPath $temporary -Destination $Path -Force
    } finally {
        Remove-Item -LiteralPath $temporary -Force -ErrorAction SilentlyContinue
    }
}

function Get-ActiveAlphaManifest {
    param([Parameter(Mandatory = $true)][string]$Root)

    $path = Join-Path $Root "active.json"
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { return $null }
    try {
        $active = Get-Content -LiteralPath $path -Raw | ConvertFrom-Json
        if ($active.build -isnot [string] -or $active.build -cnotmatch "^[0-9a-f]{40}$") { return $null }
        return $active
    } catch {
        return $null
    }
}

function Test-AlphaVersionComplete {
    param([Parameter(Mandatory = $true)][string]$Path)
    return ((Test-Path -LiteralPath (Join-Path $Path "orange.exe") -PathType Leaf) -and
        (Test-Path -LiteralPath (Join-Path $Path "orange-tray.exe") -PathType Leaf))
}

function Install-AlphaVersion {
    param(
        [Parameter(Mandatory = $true)][string]$Root,
        [Parameter(Mandatory = $true)]$Manifest,
        [switch]$AllowLocalAssets
    )

    Initialize-AlphaRoot -Root $Root
    $versions = Join-Path $Root "versions"
    $destination = Join-Path $versions $Manifest.build
    $previous = Get-ActiveAlphaManifest -Root $Root

    if (-not (Test-AlphaVersionComplete -Path $destination)) {
        if (Test-Path -LiteralPath $destination) {
            Remove-Item -LiteralPath $destination -Recurse -Force
        }

        $id = [guid]::NewGuid().ToString("N")
        $download = Join-Path $versions (".download-$id.zip")
        $extraction = Join-Path $versions (".extract-$id")
        try {
            $isLocal = Test-Path -LiteralPath $Manifest.asset_url -PathType Leaf
            if ($isLocal) {
                if (-not $AllowLocalAssets) {
                    throw "Local manifest assets require the explicit test parameter"
                }
                Copy-Item -LiteralPath $Manifest.asset_url -Destination $download
            } else {
                Invoke-WebRequest -Uri $Manifest.asset_url -OutFile $download -UseBasicParsing
            }

            $actualHash = (Get-FileHash -LiteralPath $download -Algorithm SHA256).Hash.ToUpperInvariant()
            if ($actualHash -cne $Manifest.sha256.ToUpperInvariant()) {
                throw "Asset checksum mismatch"
            }

            New-Item -ItemType Directory -Path $extraction | Out-Null
            Expand-Archive -LiteralPath $download -DestinationPath $extraction
            if (-not (Test-AlphaVersionComplete -Path $extraction)) {
                throw "Alpha asset must contain orange.exe and orange-tray.exe at its root"
            }
            Move-Item -LiteralPath $extraction -Destination $destination
        } finally {
            Remove-Item -LiteralPath $download -Force -ErrorAction SilentlyContinue
            Remove-Item -LiteralPath $extraction -Recurse -Force -ErrorAction SilentlyContinue
        }
    }

    $previousBuild = $null
    if ($null -ne $previous) {
        if ($previous.build -ne $Manifest.build) {
            $previousBuild = $previous.build
        } elseif ($previous.PSObject.Properties.Name -contains "previous_build" -and
            $previous.previous_build -is [string] -and
            $previous.previous_build -cmatch "^[0-9a-f]{40}$") {
            $previousBuild = $previous.previous_build
        }
    }
    $active = [ordered]@{
        schema = 1
        build = $Manifest.build
        previous_build = $previousBuild
        profile = $Manifest.profile
        environment = $Manifest.environment
    }
    Write-AlphaJsonAtomically -Path (Join-Path $Root "active.json") -Value $active

    $keep = @($Manifest.build)
    if ($null -ne $previousBuild) { $keep += $previousBuild }
    foreach ($directory in @(Get-ChildItem -LiteralPath $versions -Directory)) {
        if ($keep -notcontains $directory.Name) {
            Remove-Item -LiteralPath $directory.FullName -Recurse -Force
        }
    }
    return $destination
}

function New-CanonicalAlphaUuid {
    return [guid]::NewGuid().ToString("D").ToLowerInvariant()
}

function Get-AlphaDeviceId {
    param([Parameter(Mandatory = $true)][string]$Root)

    Initialize-AlphaRoot -Root $Root
    $path = Join-Path $Root "device-id.txt"
    if (Test-Path -LiteralPath $path -PathType Leaf) {
        $candidate = (Get-Content -LiteralPath $path -Raw).Trim()
        if ($candidate -cmatch "^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$") {
            return $candidate
        }
    }
    $device = New-CanonicalAlphaUuid
    Set-Content -LiteralPath $path -Value $device -Encoding ASCII
    return $device
}

function New-AlphaRun {
    param(
        [Parameter(Mandatory = $true)][string]$Root,
        [Parameter(Mandatory = $true)][string]$Build,
        [Parameter(Mandatory = $true)][string]$Device,
        [Parameter(Mandatory = $true)][string]$Profile
    )

    Initialize-AlphaRoot -Root $Root
    $run = New-CanonicalAlphaUuid
    $directory = Join-Path (Join-Path $Root "diagnostics") $run
    New-Item -ItemType Directory -Path $directory | Out-Null
    $metadata = [ordered]@{
        schema = 1
        build = $Build
        run = $run
        device = $Device
        profile = $Profile
        started_at = [DateTime]::UtcNow.ToString("o")
    }
    Write-AlphaJsonAtomically -Path (Join-Path $directory "run.json") -Value $metadata
    return [pscustomobject]@{
        Build = $Build
        Run = $run
        Device = $Device
        Profile = $Profile
        Directory = $directory
    }
}

function Set-AlphaLaunchEnvironment {
    param(
        [Parameter(Mandatory = $true)]$RunContext,
        $Environment,
        [Parameter(Mandatory = $true)][string]$Server
    )

    $values = [ordered]@{
        ORANGE_BUILD_ID = $RunContext.Build
        ORANGE_RUN_ID = $RunContext.Run
        ORANGE_DEVICE_ID = $RunContext.Device
        ORANGE_TEST_PROFILE = $RunContext.Profile
        ORANGE_MEDIA_DIAGNOSTICS = $RunContext.Directory
        ORANGE_SERVER = $Server
    }
    if ($null -ne $Environment) {
        foreach ($property in @($Environment.PSObject.Properties)) {
            if ($property.Name -cmatch "^ORANGE_[A-Z0-9_]+$" -and
                $script:AlphaReservedEnvironment -notcontains $property.Name -and
                $property.Value -is [string]) {
                $values[$property.Name] = $property.Value
            }
        }
    }

    $state = New-Object System.Collections.Generic.List[object]
    try {
        foreach ($entry in $values.GetEnumerator()) {
            $old = [Environment]::GetEnvironmentVariable($entry.Key, "Process")
            $state.Add([pscustomobject]@{ Name = $entry.Key; Exists = ($null -ne $old); Value = $old })
            [Environment]::SetEnvironmentVariable($entry.Key, [string]$entry.Value, "Process")
        }
    } catch {
        Restore-AlphaLaunchEnvironment -State $state.ToArray()
        throw
    }
    return ,$state.ToArray()
}

function Restore-AlphaLaunchEnvironment {
    param([Parameter(Mandatory = $true)]$State)

    foreach ($entry in @($State)) {
        if ($entry.Exists) {
            [Environment]::SetEnvironmentVariable($entry.Name, $entry.Value, "Process")
        } else {
            [Environment]::SetEnvironmentVariable($entry.Name, $null, "Process")
        }
    }
}

function Complete-AlphaRun {
    param(
        [Parameter(Mandatory = $true)][string]$Root,
        [Parameter(Mandatory = $true)][string]$RunDirectory
    )

    Initialize-AlphaRoot -Root $Root
    $runJson = Join-Path $RunDirectory "run.json"
    if (-not (Test-Path -LiteralPath $runJson -PathType Leaf)) {
        throw "Cannot archive a run without run.json"
    }
    $metadata = Get-Content -LiteralPath $runJson -Raw | ConvertFrom-Json
    foreach ($name in @("build", "run", "device", "profile")) {
        if ($metadata.PSObject.Properties.Name -notcontains $name -or $metadata.$name -isnot [string]) {
            throw "Invalid run metadata: missing $name"
        }
    }

    $pending = Join-Path $Root "pending"
    $zipPath = Join-Path $pending ($metadata.run + ".zip")
    $sidecarPath = Join-Path $pending ($metadata.run + ".json")
    if ((Test-Path -LiteralPath $zipPath -PathType Leaf) -and (Test-Path -LiteralPath $sidecarPath -PathType Leaf)) {
        return [pscustomobject]@{ Zip = $zipPath; Metadata = $sidecarPath }
    }

    $id = [guid]::NewGuid().ToString("N")
    $staging = Join-Path $pending (".archive-$id")
    $temporaryZip = Join-Path $pending (".archive-$id.zip")
    $temporarySidecar = Join-Path $pending (".archive-$id.json")
    try {
        New-Item -ItemType Directory -Path $staging | Out-Null
        Copy-Item -LiteralPath $runJson -Destination (Join-Path $staging "run.json")
        $prefixLength = $RunDirectory.TrimEnd("\").Length
        foreach ($file in @(Get-ChildItem -LiteralPath $RunDirectory -Recurse -File -Filter "*.jsonl")) {
            $relative = $file.FullName.Substring($prefixLength).TrimStart("\")
            $target = Join-Path $staging $relative
            New-Item -ItemType Directory -Path (Split-Path -Parent $target) -Force | Out-Null
            Copy-Item -LiteralPath $file.FullName -Destination $target
        }
        Compress-Archive -Path (Join-Path $staging "*") -DestinationPath $temporaryZip
        $sidecar = [ordered]@{
            build = $metadata.build
            run = $metadata.run
            device = $metadata.device
            profile = $metadata.profile
        }
        $sidecar | ConvertTo-Json | Set-Content -LiteralPath $temporarySidecar -Encoding UTF8
        Move-Item -LiteralPath $temporaryZip -Destination $zipPath -Force
        Move-Item -LiteralPath $temporarySidecar -Destination $sidecarPath -Force
    } finally {
        Remove-Item -LiteralPath $staging -Recurse -Force -ErrorAction SilentlyContinue
        Remove-Item -LiteralPath $temporaryZip -Force -ErrorAction SilentlyContinue
        Remove-Item -LiteralPath $temporarySidecar -Force -ErrorAction SilentlyContinue
    }
    return [pscustomobject]@{ Zip = $zipPath; Metadata = $sidecarPath }
}

function Recover-AlphaRuns {
    param([Parameter(Mandatory = $true)][string]$Root)

    Initialize-AlphaRoot -Root $Root
    $recovered = 0
    foreach ($directory in @(Get-ChildItem -LiteralPath (Join-Path $Root "diagnostics") -Directory)) {
        $pendingZip = Join-Path (Join-Path $Root "pending") ($directory.Name + ".zip")
        $pendingSidecar = Join-Path (Join-Path $Root "pending") ($directory.Name + ".json")
        $sentZip = Join-Path (Join-Path $Root "sent") ($directory.Name + ".zip")
        $sentSidecar = Join-Path (Join-Path $Root "sent") ($directory.Name + ".json")
        $pendingComplete = (Test-Path -LiteralPath $pendingZip -PathType Leaf) -and (Test-Path -LiteralPath $pendingSidecar -PathType Leaf)
        $sentComplete = (Test-Path -LiteralPath $sentZip -PathType Leaf) -and (Test-Path -LiteralPath $sentSidecar -PathType Leaf)
        if (-not $pendingComplete -and -not $sentComplete -and
            (Test-Path -LiteralPath (Join-Path $directory.FullName "run.json") -PathType Leaf)) {
            Complete-AlphaRun -Root $Root -RunDirectory $directory.FullName | Out-Null
            $recovered++
        }
    }
    return $recovered
}

function Get-AlphaUploadUri {
    param([string]$Server)

    if ([string]::IsNullOrWhiteSpace($Server)) { $Server = $script:AlphaDefaultServer }
    $uri = $null
    if (-not [Uri]::TryCreate($Server, [UriKind]::Absolute, [ref]$uri)) {
        throw "Invalid ORANGE_SERVER URL"
    }
    if ($uri.Scheme -eq "wss") { $scheme = "https" }
    elseif ($uri.Scheme -eq "ws") { $scheme = "http" }
    elseif ($uri.Scheme -eq "https" -or $uri.Scheme -eq "http") { $scheme = $uri.Scheme }
    else { throw "Unsupported ORANGE_SERVER URL scheme" }
    return ("{0}://{1}/diagnostics" -f $scheme, $uri.Authority)
}

function Invoke-PendingUploads {
    param(
        [Parameter(Mandatory = $true)][string]$Root,
        [string]$SessionPath = (Join-Path (Join-Path $env:APPDATA "orange") "session.json"),
        [string]$UploadUri,
        [string]$Server
    )

    Initialize-AlphaRoot -Root $Root
    $pending = Join-Path $Root "pending"
    $archives = @(Get-ChildItem -LiteralPath $pending -File -Filter "*.zip")
    $uploaded = 0
    if ($archives.Count -eq 0) {
        return [pscustomobject]@{ Uploaded = 0; Pending = 0 }
    }

    $token = $null
    if (Test-Path -LiteralPath $SessionPath -PathType Leaf) {
        try {
            $session = Get-Content -LiteralPath $SessionPath -Raw | ConvertFrom-Json
            if ($session.token -is [string] -and -not [string]::IsNullOrWhiteSpace($session.token)) {
                $token = $session.token
            }
        } catch {
            $token = $null
        }
    }
    if ($null -eq $token) {
        return [pscustomobject]@{ Uploaded = 0; Pending = $archives.Count }
    }
    if ([string]::IsNullOrWhiteSpace($UploadUri)) {
        try {
            if ([string]::IsNullOrWhiteSpace($Server)) { $Server = $env:ORANGE_SERVER }
            $UploadUri = Get-AlphaUploadUri -Server $Server
        } catch {
            return [pscustomobject]@{ Uploaded = 0; Pending = $archives.Count }
        }
    }

    $sent = Join-Path $Root "sent"
    foreach ($archive in $archives) {
        $sidecar = Join-Path $pending ($archive.BaseName + ".json")
        if (-not (Test-Path -LiteralPath $sidecar -PathType Leaf)) { continue }
        try {
            $metadata = Get-Content -LiteralPath $sidecar -Raw | ConvertFrom-Json
            $headers = @{
                Authorization = "Bearer $token"
                "x-orange-build" = [string]$metadata.build
                "x-orange-run" = [string]$metadata.run
                "x-orange-device" = [string]$metadata.device
                "x-orange-profile" = [string]$metadata.profile
            }
            $response = Invoke-WebRequest -Uri $UploadUri -Method Post -InFile $archive.FullName -ContentType "application/zip" -Headers $headers -UseBasicParsing
            if ([int]$response.StatusCode -eq 201) {
                Move-Item -LiteralPath $sidecar -Destination (Join-Path $sent (Split-Path -Leaf $sidecar)) -Force
                Move-Item -LiteralPath $archive.FullName -Destination (Join-Path $sent $archive.Name) -Force
                $uploaded++
            }
        } catch {
            # Upload failure is deliberately non-fatal; the pending pair is retried later.
        }
    }
    $remaining = @(Get-ChildItem -LiteralPath $pending -File -Filter "*.zip").Count
    return [pscustomobject]@{ Uploaded = $uploaded; Pending = $remaining }
}

function Invoke-OrangeAlpha {
    param(
        [Parameter(Mandatory = $true)][string]$Root,
        [string]$ManifestPath,
        [switch]$AllowLocalAssets
    )

    Initialize-AlphaRoot -Root $Root
    Recover-AlphaRuns -Root $Root | Out-Null
    $before = Invoke-PendingUploads -Root $Root

    try {
        $manifest = Get-AlphaManifest -ManifestPath $ManifestPath -AllowLocalAssets:$AllowLocalAssets
        $version = Install-AlphaVersion -Root $Root -Manifest $manifest -AllowLocalAssets:$AllowLocalAssets
        $active = Get-ActiveAlphaManifest -Root $Root
    } catch {
        $active = Get-ActiveAlphaManifest -Root $Root
        if ($null -eq $active) { throw }
        $version = Join-Path (Join-Path $Root "versions") $active.build
        if (-not (Test-AlphaVersionComplete -Path $version)) { throw }
        Write-Warning "Update failed; launching the previous active build."
    }

    $device = Get-AlphaDeviceId -Root $Root
    $run = New-AlphaRun -Root $Root -Build $active.build -Device $device -Profile $active.profile
    Write-Host ("Orange Alpha build={0} profile={1} run={2}" -f $run.Build, $run.Profile, $run.Run)

    $server = if ([string]::IsNullOrWhiteSpace($env:ORANGE_SERVER)) { $script:AlphaDefaultServer } else { $env:ORANGE_SERVER }
    $manifestServer = $active.environment.PSObject.Properties["ORANGE_SERVER"]
    if ($null -ne $manifestServer) { $server = [string]$manifestServer.Value }
    $environmentState = $null
    $runError = $null
    try {
        $environmentState = Set-AlphaLaunchEnvironment -RunContext $run -Environment $active.environment -Server $server
        Start-Process -FilePath (Join-Path $version "orange-tray.exe") -WorkingDirectory $version -Wait | Out-Null
    } catch {
        $runError = $_
    } finally {
        if ($null -ne $environmentState) { Restore-AlphaLaunchEnvironment -State $environmentState }
    }
    try {
        Complete-AlphaRun -Root $Root -RunDirectory $run.Directory | Out-Null
    } catch {
        if ($null -eq $runError) { $runError = $_ }
    }

    $after = Invoke-PendingUploads -Root $Root -Server $server
    Write-Host ("Diagnostics uploaded={0} pending={1}" -f ($before.Uploaded + $after.Uploaded), $after.Pending)
    if ($null -ne $runError) { throw $runError }
}

if ($MyInvocation.InvocationName -ne ".") {
    Invoke-OrangeAlpha -Root $Root -ManifestPath $ManifestPath -AllowLocalAssets:$AllowLocalAssets
}
