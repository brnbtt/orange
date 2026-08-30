$ErrorActionPreference = "Stop"

$script:Passed = 0
$script:Failed = 0
$script:TempRoots = New-Object System.Collections.Generic.List[string]

function Assert-True {
    param([bool]$Condition, [string]$Message)
    if (-not $Condition) { throw $Message }
}

function Assert-Equal {
    param($Expected, $Actual, [string]$Message)
    if ($Expected -ne $Actual) {
        throw "$Message (expected '$Expected', got '$Actual')"
    }
}

function Assert-Throws {
    param([scriptblock]$Action, [string]$Pattern)
    try {
        & $Action
    } catch {
        if ($_.Exception.Message -notmatch $Pattern) {
            throw "Expected error matching '$Pattern', got '$($_.Exception.Message)'"
        }
        return
    }
    throw "Expected error matching '$Pattern', but no error was raised"
}

function Invoke-Test {
    param([string]$Name, [scriptblock]$Action)
    try {
        & $Action
        $script:Passed++
        Write-Host "PASS $Name"
    } catch {
        $script:Failed++
        Write-Host "FAIL $Name`: $($_.Exception.Message)" -ForegroundColor Red
    }
}

function New-TestRoot {
    $path = Join-Path ([System.IO.Path]::GetTempPath()) ("orange-alpha-test-" + [guid]::NewGuid().ToString("N"))
    New-Item -ItemType Directory -Path $path | Out-Null
    $script:TempRoots.Add($path)
    return $path
}

function New-FixtureAsset {
    param([string]$Root, [string]$Name)
    $source = Join-Path $Root ("asset-" + $Name)
    $zip = Join-Path $Root ("asset-" + $Name + ".zip")
    New-Item -ItemType Directory -Path $source | Out-Null
    Set-Content -LiteralPath (Join-Path $source "orange.exe") -Value "orange-$Name" -Encoding ASCII
    Set-Content -LiteralPath (Join-Path $source "orange-tray.exe") -Value "tray-$Name" -Encoding ASCII
    Compress-Archive -Path (Join-Path $source "*") -DestinationPath $zip
    return [pscustomobject]@{
        Path = $zip
        Hash = (Get-FileHash -LiteralPath $zip -Algorithm SHA256).Hash.ToUpperInvariant()
    }
}

function New-AdversarialAsset {
    param([string]$Root, [string]$Name, [string]$EntryName)
    Add-Type -AssemblyName System.IO.Compression
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $zipPath = Join-Path $Root ("adversarial-" + $Name + ".zip")
    $zip = [System.IO.Compression.ZipFile]::Open($zipPath, [System.IO.Compression.ZipArchiveMode]::Create)
    try {
        foreach ($entryNameToWrite in @("orange.exe", "orange-tray.exe", $EntryName)) {
            $entry = $zip.CreateEntry($entryNameToWrite)
            $writer = New-Object System.IO.StreamWriter($entry.Open())
            try { $writer.Write("fixture") } finally { $writer.Dispose() }
        }
    } finally {
        $zip.Dispose()
    }
    return [pscustomobject]@{
        Path = $zipPath
        Hash = (Get-FileHash -LiteralPath $zipPath -Algorithm SHA256).Hash.ToUpperInvariant()
    }
}

function New-ManifestJson {
    param(
        [string]$Build = "0123456789abcdef0123456789abcdef01234567",
        [string]$AssetUrl,
        [string]$Hash,
        [string]$Profile = "hardware-bounded-jitter",
        [object]$Environment = $null,
        [int]$Schema = 1
    )
    if ($null -eq $Environment) {
        $Environment = [ordered]@{
            ORANGE_AV1_DECODER = "hardware"
            ORANGE_RTP_BUFFER_MODE = ""
        }
    }
    return ([ordered]@{
        schema = $Schema
        build = $Build
        asset_url = $AssetUrl
        sha256 = $Hash
        profile = $Profile
        environment = $Environment
    } | ConvertTo-Json -Depth 5)
}

function Write-ManifestFixture {
    param([string]$Root, [string]$Json, [string]$Name = "manifest.json")
    $path = Join-Path $Root $Name
    Set-Content -LiteralPath $path -Value $Json -Encoding UTF8
    return $path
}

function Read-ZipEntryNames {
    param([string]$Path)
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $zip = [System.IO.Compression.ZipFile]::OpenRead($Path)
    try { return @($zip.Entries | ForEach-Object { $_.FullName.Replace("\", "/") }) }
    finally { $zip.Dispose() }
}

$launcher = Join-Path $PSScriptRoot "orange-alpha.ps1"
if (-not (Test-Path -LiteralPath $launcher -PathType Leaf)) {
    throw "Expected RED: launcher functions are absent ($launcher)"
}
. $launcher

try {
    Invoke-Test "valid manifest parsing normalizes the checksum" {
        $root = New-TestRoot
        $asset = New-FixtureAsset $root "valid"
        $manifest = Read-AlphaManifest -Path (Write-ManifestFixture $root (New-ManifestJson -AssetUrl $asset.Path -Hash $asset.Hash.ToLowerInvariant())) -AllowLocalAssets
        Assert-Equal 1 $manifest.schema "schema"
        Assert-Equal $asset.Hash $manifest.sha256 "normalized hash"
        Assert-Equal "hardware" $manifest.environment.ORANGE_AV1_DECODER "environment"
    }

    Invoke-Test "invalid manifest schema, build, checksum, profile, URL, and environment are rejected" {
        $root = New-TestRoot
        $asset = New-FixtureAsset $root "invalid"
        $base = @{ AssetUrl = $asset.Path; Hash = $asset.Hash }
        Assert-Throws { Read-AlphaManifest -Path (Write-ManifestFixture $root (New-ManifestJson @base -Schema 2) "schema.json") -AllowLocalAssets } "schema"
        Assert-Throws { Read-AlphaManifest -Path (Write-ManifestFixture $root (New-ManifestJson @base -Build "abc") "build.json") -AllowLocalAssets } "build"
        Assert-Throws { Read-AlphaManifest -Path (Write-ManifestFixture $root (New-ManifestJson -AssetUrl $asset.Path -Hash "xyz") "hash.json") -AllowLocalAssets } "sha256"
        Assert-Throws { Read-AlphaManifest -Path (Write-ManifestFixture $root (New-ManifestJson @base -Profile "Bad Profile") "profile.json") -AllowLocalAssets } "profile"
        Assert-Throws { Read-AlphaManifest -Path (Write-ManifestFixture $root (New-ManifestJson -AssetUrl "http://example.test/orange.zip" -Hash $asset.Hash) "url.json") } "HTTPS"
        Assert-Throws { Read-AlphaManifest -Path (Write-ManifestFixture $root (New-ManifestJson @base -Environment @{ PATH = "bad" }) "env-name.json") -AllowLocalAssets } "environment"
        Assert-Throws { Read-AlphaManifest -Path (Write-ManifestFixture $root (New-ManifestJson @base -Environment @{ ORANGE_BUILD_ID = "override" }) "env-override.json") -AllowLocalAssets } "reserved"
        Assert-Throws { Read-AlphaManifest -Path (Write-ManifestFixture $root (New-ManifestJson @base -Environment @{ ORANGE_TEST = 4 }) "env-value.json") -AllowLocalAssets } "string"
        Assert-Throws { Read-AlphaManifest -Path (Write-ManifestFixture $root (New-ManifestJson @base -Environment 4) "env-scalar.json") -AllowLocalAssets } "environment"
    }

    Invoke-Test "manifest field names are exact and case-sensitive" {
        $root = New-TestRoot
        $asset = New-FixtureAsset $root "field-names"
        $json = New-ManifestJson -AssetUrl $asset.Path -Hash $asset.Hash
        $missing = $json | ConvertFrom-Json
        $missing.PSObject.Properties.Remove("profile")
        Assert-Throws { Read-AlphaManifest -Path (Write-ManifestFixture $root ($missing | ConvertTo-Json -Depth 5) "missing.json") -AllowLocalAssets } "exactly"
        $additional = $json.TrimEnd() -replace '}\s*$', ',"unexpected":true}'
        Assert-Throws { Read-AlphaManifest -Path (Write-ManifestFixture $root $additional "additional.json") -AllowLocalAssets } "exactly"
        $incorrectCase = $json -creplace '"schema"', '"Schema"'
        Assert-Throws { Read-AlphaManifest -Path (Write-ManifestFixture $root $incorrectCase "case.json") -AllowLocalAssets } "exactly"
        $surfacedDuplicate = $json -creplace '"schema"\s*:\s*1', '"schema":1,"Schema":1'
        Assert-Throws { Read-AlphaManifest -Path (Write-ManifestFixture $root $surfacedDuplicate "duplicate.json") -AllowLocalAssets } "Invalid manifest JSON"
    }

    Invoke-Test "first install activates a complete version" {
        $root = New-TestRoot
        $asset = New-FixtureAsset $root "first"
        $manifest = Read-AlphaManifest -Path (Write-ManifestFixture $root (New-ManifestJson -AssetUrl $asset.Path -Hash $asset.Hash)) -AllowLocalAssets
        $version = Install-AlphaVersion -Root $root -Manifest $manifest -AllowLocalAssets
        Assert-True (Test-Path -LiteralPath (Join-Path $version "orange.exe")) "orange.exe missing"
        Assert-True (Test-Path -LiteralPath (Join-Path $version "orange-tray.exe")) "orange-tray.exe missing"
        Assert-Equal $manifest.build ((Get-Content -LiteralPath (Join-Path $root "active.json") -Raw | ConvertFrom-Json).build) "active build"
    }

    Invoke-Test "cache hit does not read the asset again" {
        $root = New-TestRoot
        $asset = New-FixtureAsset $root "cache"
        $manifest = Read-AlphaManifest -Path (Write-ManifestFixture $root (New-ManifestJson -AssetUrl $asset.Path -Hash $asset.Hash)) -AllowLocalAssets
        $first = Install-AlphaVersion -Root $root -Manifest $manifest -AllowLocalAssets
        Remove-Item -LiteralPath $asset.Path -Force
        $second = Install-AlphaVersion -Root $root -Manifest $manifest -AllowLocalAssets
        Assert-Equal $first $second "cached version path"
    }

    Invoke-Test "changed build keeps the active and immediately previous versions" {
        $root = New-TestRoot
        $builds = @(
            "1111111111111111111111111111111111111111",
            "2222222222222222222222222222222222222222",
            "3333333333333333333333333333333333333333"
        )
        foreach ($build in $builds) {
            $asset = New-FixtureAsset $root $build.Substring(0, 1)
            $manifest = Read-AlphaManifest -Path (Write-ManifestFixture $root (New-ManifestJson -Build $build -AssetUrl $asset.Path -Hash $asset.Hash) ("$build.json")) -AllowLocalAssets
            Install-AlphaVersion -Root $root -Manifest $manifest -AllowLocalAssets | Out-Null
        }
        Remove-Item -LiteralPath $asset.Path -Force
        Install-AlphaVersion -Root $root -Manifest $manifest -AllowLocalAssets | Out-Null
        $versions = @(Get-ChildItem -LiteralPath (Join-Path $root "versions") -Directory | Select-Object -ExpandProperty Name | Sort-Object)
        Assert-Equal 2 $versions.Count "retained version count"
        Assert-Equal $builds[1] $versions[0] "previous version"
        Assert-Equal $builds[2] $versions[1] "active version"
    }

    Invoke-Test "active replacement retains and can recover the prior state" {
        $root = New-TestRoot
        $firstBuild = "4444444444444444444444444444444444444444"
        $secondBuild = "5555555555555555555555555555555555555555"
        $firstAsset = New-FixtureAsset $root "atomic-first"
        $first = Read-AlphaManifest -Path (Write-ManifestFixture $root (New-ManifestJson -Build $firstBuild -AssetUrl $firstAsset.Path -Hash $firstAsset.Hash) "atomic-first.json") -AllowLocalAssets
        Install-AlphaVersion -Root $root -Manifest $first -AllowLocalAssets | Out-Null
        $secondAsset = New-FixtureAsset $root "atomic-second"
        $second = Read-AlphaManifest -Path (Write-ManifestFixture $root (New-ManifestJson -Build $secondBuild -AssetUrl $secondAsset.Path -Hash $secondAsset.Hash) "atomic-second.json") -AllowLocalAssets
        Install-AlphaVersion -Root $root -Manifest $second -AllowLocalAssets | Out-Null
        $backupPath = Join-Path $root "active.json.bak"
        Assert-True (Test-Path -LiteralPath $backupPath -PathType Leaf) "active backup missing"
        Assert-Equal $firstBuild ((Get-Content -LiteralPath $backupPath -Raw | ConvertFrom-Json).build) "backup build"
        Set-Content -LiteralPath (Join-Path $root "active.json") -Value "corrupt" -Encoding ASCII
        Assert-Equal $firstBuild (Get-ActiveAlphaManifest -Root $root).build "backup recovery build"
    }

    Invoke-Test "unsafe ZIP entries are rejected before extraction" {
        $entries = @(
            @{ Name = "parent"; Entry = "../escaped.txt" },
            @{ Name = "absolute"; Entry = "/orange-alpha-absolute.txt" },
            @{ Name = "drive"; Entry = "C:/orange-alpha-drive.txt" }
        )
        foreach ($case in $entries) {
            $root = New-TestRoot
            $asset = New-AdversarialAsset -Root $root -Name $case.Name -EntryName $case.Entry
            $build = if ($case.Name -eq "parent") { "6" * 40 } elseif ($case.Name -eq "absolute") { "7" * 40 } else { "8" * 40 }
            $manifest = Read-AlphaManifest -Path (Write-ManifestFixture $root (New-ManifestJson -Build $build -AssetUrl $asset.Path -Hash $asset.Hash)) -AllowLocalAssets
            Assert-Throws { Install-AlphaVersion -Root $root -Manifest $manifest -AllowLocalAssets } "unsafe ZIP entry"
            Assert-True (-not (Test-Path -LiteralPath (Join-Path (Join-Path $root "versions") "escaped.txt"))) "parent entry escaped extraction root"
            Assert-True (-not (Test-Path -LiteralPath (Join-Path (Join-Path $root "versions") $build))) "unsafe build was activated"
        }
    }

    Invoke-Test "checksum rejection preserves the active build" {
        $root = New-TestRoot
        $firstAsset = New-FixtureAsset $root "good"
        $first = Read-AlphaManifest -Path (Write-ManifestFixture $root (New-ManifestJson -AssetUrl $firstAsset.Path -Hash $firstAsset.Hash)) -AllowLocalAssets
        Install-AlphaVersion -Root $root -Manifest $first -AllowLocalAssets | Out-Null
        $badAsset = New-FixtureAsset $root "bad"
        $nextBuild = "abcdefabcdefabcdefabcdefabcdefabcdefabcd"
        $bad = Read-AlphaManifest -Path (Write-ManifestFixture $root (New-ManifestJson -Build $nextBuild -AssetUrl $badAsset.Path -Hash ("0" * 64)) "bad.json") -AllowLocalAssets
        Assert-Throws { Install-AlphaVersion -Root $root -Manifest $bad -AllowLocalAssets } "checksum"
        Assert-Equal $first.build ((Get-Content -LiteralPath (Join-Path $root "active.json") -Raw | ConvertFrom-Json).build) "active after rejection"
        Assert-True (-not (Test-Path -LiteralPath (Join-Path (Join-Path $root "versions") $nextBuild))) "rejected build was retained"
    }

    Invoke-Test "device ID persists and run IDs are fresh canonical UUIDs" {
        $root = New-TestRoot
        $device1 = Get-AlphaDeviceId -Root $root
        $device2 = Get-AlphaDeviceId -Root $root
        $run1 = New-AlphaRun -Root $root -Build ("a" * 40) -Device $device1 -Profile "test-profile"
        $run2 = New-AlphaRun -Root $root -Build ("a" * 40) -Device $device1 -Profile "test-profile"
        Assert-Equal $device1 $device2 "persistent device ID"
        Assert-True ($device1 -cmatch "^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$") "device ID is not canonical lowercase"
        Assert-True ($run1.Run -ne $run2.Run) "run IDs were reused"
        Assert-True ($run1.Run -cmatch "^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$") "run ID is not canonical lowercase"
        $metadata = Get-Content -LiteralPath (Join-Path $run1.Directory "run.json") -Raw | ConvertFrom-Json
        Assert-Equal 1 $metadata.schema "run schema"
        Assert-True ($metadata.started_at -match "Z$") "start time is not UTC"
    }

    Invoke-Test "launch environment is constrained and restored" {
        $root = New-TestRoot
        $run = New-AlphaRun -Root $root -Build ("b" * 40) -Device ([guid]::NewGuid().ToString("D").ToLowerInvariant()) -Profile "test-profile"
        $oldBuild = [Environment]::GetEnvironmentVariable("ORANGE_BUILD_ID", "Process")
        $oldPath = $env:PATH
        $env:ORANGE_BUILD_ID = "prior"
        $state = Set-AlphaLaunchEnvironment -RunContext $run -Environment ([pscustomobject]@{ ORANGE_AV1_DECODER = "hardware"; PATH = "ignored" }) -Server "ws://127.0.0.1:9000/ws"
        try {
            Assert-Equal $run.Build $env:ORANGE_BUILD_ID "build environment"
            Assert-Equal $run.Run $env:ORANGE_RUN_ID "run environment"
            Assert-Equal $run.Directory $env:ORANGE_MEDIA_DIAGNOSTICS "diagnostics environment"
            Assert-Equal "hardware" $env:ORANGE_AV1_DECODER "manifest environment"
            Assert-Equal $oldPath $env:PATH "non-Orange environment was changed"
        } finally {
            Restore-AlphaLaunchEnvironment -State $state
        }
        Assert-Equal "prior" $env:ORANGE_BUILD_ID "prior environment was not restored"
        if ($null -eq $oldBuild) { Remove-Item Env:ORANGE_BUILD_ID -ErrorAction SilentlyContinue } else { $env:ORANGE_BUILD_ID = $oldBuild }
    }

    Invoke-Test "archive and sidecar contain only permitted diagnostics metadata" {
        $root = New-TestRoot
        $device = [guid]::NewGuid().ToString("D").ToLowerInvariant()
        $run = New-AlphaRun -Root $root -Build ("c" * 40) -Device $device -Profile "test-profile"
        New-Item -ItemType Directory -Path (Join-Path $run.Directory "nested") | Out-Null
        Set-Content -LiteralPath (Join-Path $run.Directory "media.jsonl") -Value '{"event":"ok"}' -Encoding UTF8
        Set-Content -LiteralPath (Join-Path $run.Directory "nested\child.jsonl") -Value '{"event":"child"}' -Encoding UTF8
        Set-Content -LiteralPath (Join-Path $run.Directory "session.json") -Value '{"token":"secret"}' -Encoding UTF8
        Set-Content -LiteralPath (Join-Path $run.Directory "notes.txt") -Value 'private' -Encoding UTF8
        $archive = Complete-AlphaRun -Root $root -RunDirectory $run.Directory
        $names = @(Read-ZipEntryNames $archive.Zip)
        Assert-True ($names -contains "run.json") "run.json missing"
        Assert-True ($names -contains "media.jsonl") "JSONL missing"
        Assert-True ($names -contains "nested/child.jsonl") "nested JSONL missing"
        Assert-True (-not ($names -contains "session.json")) "session.json was archived"
        Assert-True (-not ($names -contains "notes.txt")) "non-diagnostic file was archived"
        $sidecar = Get-Content -LiteralPath $archive.Metadata -Raw | ConvertFrom-Json
        Assert-Equal $run.Run $sidecar.run "sidecar run"
        Assert-Equal $device $sidecar.device "sidecar device"
    }

    Invoke-Test "corrupt run metadata cannot traverse or overwrite active state" {
        $root = New-TestRoot
        $activePath = Join-Path $root "active.json"
        Set-Content -LiteralPath $activePath -Value '{"sentinel":"unchanged"}' -Encoding ASCII
        $run = New-AlphaRun -Root $root -Build ("c" * 40) -Device ([guid]::NewGuid().ToString("D").ToLowerInvariant()) -Profile "test-profile"
        $metadata = Get-Content -LiteralPath (Join-Path $run.Directory "run.json") -Raw | ConvertFrom-Json
        $metadata.run = "..\active"
        $metadata | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $run.Directory "run.json") -Encoding UTF8
        Assert-Throws { Complete-AlphaRun -Root $root -RunDirectory $run.Directory } "run metadata"
        Assert-Equal '{"sentinel":"unchanged"}' ((Get-Content -LiteralPath $activePath -Raw).Trim()) "active state was overwritten"
        Assert-True (-not (Test-Path -LiteralPath (Join-Path $root "active.zip"))) "traversal ZIP was created"

        $outsideName = "orange-alpha-outside-" + [guid]::NewGuid().ToString("N")
        $outsideJson = Join-Path ([IO.Path]::GetTempPath()) ($outsideName + ".json")
        $outsideZip = Join-Path ([IO.Path]::GetTempPath()) ($outsideName + ".zip")
        $run2 = New-AlphaRun -Root $root -Build ("c" * 40) -Device ([guid]::NewGuid().ToString("D").ToLowerInvariant()) -Profile "test-profile"
        $metadata2 = Get-Content -LiteralPath (Join-Path $run2.Directory "run.json") -Raw | ConvertFrom-Json
        $metadata2.run = "..\..\$outsideName"
        $metadata2 | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $run2.Directory "run.json") -Encoding UTF8
        try {
            Assert-Throws { Complete-AlphaRun -Root $root -RunDirectory $run2.Directory } "run metadata"
            Assert-True (-not (Test-Path -LiteralPath $outsideJson)) "outside sidecar was created"
            Assert-True (-not (Test-Path -LiteralPath $outsideZip)) "outside ZIP was created"
        } finally {
            Remove-Item -LiteralPath $outsideJson, $outsideZip -Force -ErrorAction SilentlyContinue
        }
    }

    Invoke-Test "run metadata schema and values are validated completely" {
        $mutations = @(
            @{ Name = "schema"; Apply = { param($m) $m.schema = 2 } },
            @{ Name = "build"; Apply = { param($m) $m.build = "ABC" } },
            @{ Name = "run"; Apply = { param($m) $m.run = [guid]::NewGuid().ToString("D").ToUpperInvariant() } },
            @{ Name = "device"; Apply = { param($m) $m.device = "not-a-uuid" } },
            @{ Name = "profile"; Apply = { param($m) $m.profile = "Bad Profile" } },
            @{ Name = "started"; Apply = { param($m) $m.started_at = "not-utc" } },
            @{ Name = "missing"; Apply = { param($m) $m.PSObject.Properties.Remove("profile") } },
            @{ Name = "case"; Apply = { param($m) $value = $m.schema; $m.PSObject.Properties.Remove("schema"); $m | Add-Member -NotePropertyName Schema -NotePropertyValue $value } },
            @{ Name = "extra"; Apply = { param($m) $m | Add-Member -NotePropertyName unexpected -NotePropertyValue $true } }
        )
        foreach ($mutation in $mutations) {
            $root = New-TestRoot
            $run = New-AlphaRun -Root $root -Build ("d" * 40) -Device ([guid]::NewGuid().ToString("D").ToLowerInvariant()) -Profile "test-profile"
            $metadata = Get-Content -LiteralPath (Join-Path $run.Directory "run.json") -Raw | ConvertFrom-Json
            & $mutation.Apply $metadata
            $metadata | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $run.Directory "run.json") -Encoding UTF8
            Assert-Throws { Complete-AlphaRun -Root $root -RunDirectory $run.Directory } "run metadata"
            Assert-Equal 0 @(Get-ChildItem -LiteralPath (Join-Path $root "pending") -File).Count ("pending files for " + $mutation.Name)
        }
    }

    Invoke-Test "startup recovers an unarchived prior run once" {
        $root = New-TestRoot
        $run = New-AlphaRun -Root $root -Build ("d" * 40) -Device ([guid]::NewGuid().ToString("D").ToLowerInvariant()) -Profile "test-profile"
        Set-Content -LiteralPath (Join-Path $run.Directory "crash.jsonl") -Value '{"event":"before-crash"}' -Encoding UTF8
        Assert-Equal 1 (Recover-AlphaRuns -Root $root) "recovered count"
        Assert-True (Test-Path -LiteralPath (Join-Path (Join-Path $root "pending") ($run.Run + ".zip"))) "recovered archive missing"
        Assert-Equal 0 (Recover-AlphaRuns -Root $root) "duplicate recovery count"
        Remove-Item -LiteralPath (Join-Path (Join-Path $root "pending") ($run.Run + ".json")) -Force
        Assert-Equal 1 (Recover-AlphaRuns -Root $root) "incomplete pending pair recovery count"
        Assert-True (Test-Path -LiteralPath (Join-Path (Join-Path $root "pending") ($run.Run + ".json"))) "recovered sidecar missing"
    }

    Invoke-Test "recovery isolates a corrupt run and continues with valid runs" {
        $root = New-TestRoot
        $corruptRun = [guid]::NewGuid().ToString("D").ToLowerInvariant()
        $corruptDirectory = Join-Path (Join-Path $root "diagnostics") $corruptRun
        New-Item -ItemType Directory -Path $corruptDirectory -Force | Out-Null
        Set-Content -LiteralPath (Join-Path $corruptDirectory "run.json") -Value '{not-json' -Encoding ASCII
        $valid = New-AlphaRun -Root $root -Build ("e" * 40) -Device ([guid]::NewGuid().ToString("D").ToLowerInvariant()) -Profile "test-profile"
        Set-Content -LiteralPath (Join-Path $valid.Directory "valid.jsonl") -Value '{"event":"valid"}' -Encoding UTF8
        Assert-Equal 1 (Recover-AlphaRuns -Root $root) "valid recovery count"
        Assert-True (Test-Path -LiteralPath (Join-Path $corruptDirectory "run.json")) "corrupt run was removed"
        Assert-True (Test-Path -LiteralPath (Join-Path (Join-Path $root "pending") ($valid.Run + ".zip"))) "valid run was not recovered"
    }

    Invoke-Test "named mutex serializes launcher mutations and releases after errors" {
        $root = New-TestRoot
        $asset = New-FixtureAsset $root "mutex-install"
        $build = "9" * 40
        $manifest = Read-AlphaManifest -Path (Write-ManifestFixture $root (New-ManifestJson -Build $build -AssetUrl $asset.Path -Hash $asset.Hash)) -AllowLocalAssets
        $unarchived = New-AlphaRun -Root $root -Build ("a" * 40) -Device ([guid]::NewGuid().ToString("D").ToLowerInvariant()) -Profile "test-profile"
        $ready = Join-Path $root "mutex-ready.txt"
        $release = Join-Path $root "mutex-release.txt"
        $job = Start-Job -ArgumentList $launcher, $root, $ready, $release -ScriptBlock {
            param($Launcher, $StateRoot, $Ready, $Release)
            . $Launcher
            Invoke-WithAlphaMutex -Root $StateRoot -TimeoutMilliseconds 2000 -Action {
                Set-Content -LiteralPath $Ready -Value (Get-AlphaMutexName -Root $StateRoot) -Encoding ASCII
                $deadline = [DateTime]::UtcNow.AddSeconds(10)
                while (-not (Test-Path -LiteralPath $Release) -and [DateTime]::UtcNow -lt $deadline) {
                    Start-Sleep -Milliseconds 25
                }
            }
        }
        try {
            $deadline = [DateTime]::UtcNow.AddSeconds(5)
            while (-not (Test-Path -LiteralPath $ready) -and [DateTime]::UtcNow -lt $deadline) {
                Start-Sleep -Milliseconds 25
            }
            Assert-True (Test-Path -LiteralPath $ready) "mutex holder did not acquire the lock"
            Assert-Equal (Get-AlphaMutexName -Root $root) ((Get-Content -LiteralPath $ready -Raw).Trim()) "mutex names differ across processes"
            Assert-Equal "Running" $job.State "mutex holder exited before contention test"
            $watch = [Diagnostics.Stopwatch]::StartNew()
            Assert-Throws { Invoke-WithAlphaMutex -Root $root -TimeoutMilliseconds 200 -Action { throw "unexpected acquisition" } } "Timed out"
            $watch.Stop()
            Assert-True ($watch.ElapsedMilliseconds -lt 1500) "mutex acquisition was not bounded"
            Assert-Throws { Install-AlphaVersion -Root $root -Manifest $manifest -AllowLocalAssets -LockTimeoutMilliseconds 200 } "Timed out"
            Assert-Equal 0 (Recover-AlphaRuns -Root $root -LockTimeoutMilliseconds 200) "recovery mutated state while locked"
            Assert-True (-not (Test-Path -LiteralPath (Join-Path (Join-Path $root "versions") $build))) "install bypassed the mutex"
            Assert-True (-not (Test-Path -LiteralPath (Join-Path (Join-Path $root "pending") ($unarchived.Run + ".zip")))) "recovery bypassed the mutex"
            Set-Content -LiteralPath $release -Value "release" -Encoding ASCII
            Wait-Job -Job $job -Timeout 5 | Out-Null
            Receive-Job -Job $job -ErrorAction Stop | Out-Null
        } finally {
            Set-Content -LiteralPath $release -Value "release" -Encoding ASCII -ErrorAction SilentlyContinue
            Remove-Job -Job $job -Force -ErrorAction SilentlyContinue
        }
        Assert-Throws { Invoke-WithAlphaMutex -Root $root -TimeoutMilliseconds 1000 -Action { throw "fixture action failure" } } "fixture action failure"
        $marker = Join-Path $root "mutex-reacquired.txt"
        Invoke-WithAlphaMutex -Root $root -TimeoutMilliseconds 1000 -Action { Set-Content -LiteralPath $marker -Value "ok" -Encoding ASCII }
        Assert-True (Test-Path -LiteralPath $marker) "mutex was not released in finally"
        Install-AlphaVersion -Root $root -Manifest $manifest -AllowLocalAssets | Out-Null
        Assert-Equal 1 (Recover-AlphaRuns -Root $root) "recovery did not proceed after lock release"
    }

    Invoke-Test "failed upload remains pending" {
        $root = New-TestRoot
        $run = New-AlphaRun -Root $root -Build ("e" * 40) -Device ([guid]::NewGuid().ToString("D").ToLowerInvariant()) -Profile "test-profile"
        $archive = Complete-AlphaRun -Root $root -RunDirectory $run.Directory
        $session = Join-Path $root "session.json"
        Set-Content -LiteralPath $session -Value '{"token":"fixture-token","other":"ignored"}' -Encoding UTF8
        $result = Invoke-PendingUploads -Root $root -SessionPath $session -UploadUri "http://127.0.0.1:1/diagnostics"
        Assert-Equal 0 $result.Uploaded "failed upload count"
        Assert-Equal 1 $result.Pending "pending upload count"
        Assert-True (Test-Path -LiteralPath $archive.Zip) "failed ZIP was removed"
        Assert-True (Test-Path -LiteralPath $archive.Metadata) "failed sidecar was removed"
        $oldServer = [Environment]::GetEnvironmentVariable("ORANGE_SERVER", "Process")
        $env:ORANGE_SERVER = "not-a-server-url"
        try {
            $invalidServerResult = Invoke-PendingUploads -Root $root -SessionPath $session
        } finally {
            [Environment]::SetEnvironmentVariable("ORANGE_SERVER", $oldServer, "Process")
        }
        Assert-Equal 0 $invalidServerResult.Uploaded "invalid server upload count"
        Assert-Equal 1 $invalidServerResult.Pending "invalid server pending count"
    }

    Invoke-Test "201 upload moves the ZIP and sidecar to sent with exact headers" {
        $root = New-TestRoot
        $run = New-AlphaRun -Root $root -Build ("f" * 40) -Device ([guid]::NewGuid().ToString("D").ToLowerInvariant()) -Profile "test-profile"
        $archive = Complete-AlphaRun -Root $root -RunDirectory $run.Directory
        $session = Join-Path $root "session.json"
        Set-Content -LiteralPath $session -Value '{"token":"fixture-token","other":"do-not-send"}' -Encoding UTF8

        $reservation = New-Object System.Net.Sockets.TcpListener([System.Net.IPAddress]::Loopback, 0)
        $reservation.Start()
        $port = ([System.Net.IPEndPoint]$reservation.LocalEndpoint).Port
        $reservation.Stop()
        $capture = Join-Path $root "request.txt"
        $job = Start-Job -ArgumentList $port, $capture -ScriptBlock {
            param($Port, $Capture)
            $listener = New-Object System.Net.Sockets.TcpListener([System.Net.IPAddress]::Loopback, $Port)
            $listener.Start()
            try {
                $client = $listener.AcceptTcpClient()
                try {
                    $stream = $client.GetStream()
                    $reader = New-Object System.IO.StreamReader($stream, [Text.Encoding]::ASCII, $false, 4096, $true)
                    $lines = New-Object System.Collections.Generic.List[string]
                    $contentLength = 0
                    while (($line = $reader.ReadLine()) -ne "") {
                        $lines.Add($line)
                        if ($line -match '^Content-Length:\s*(\d+)$') { $contentLength = [int]$Matches[1] }
                    }
                    $remaining = $contentLength
                    $buffer = New-Object byte[] 8192
                    while ($remaining -gt 0) {
                        $read = $stream.Read($buffer, 0, [Math]::Min($buffer.Length, $remaining))
                        if ($read -le 0) { break }
                        $remaining -= $read
                    }
                    [IO.File]::WriteAllLines($Capture, $lines, [Text.Encoding]::UTF8)
                    $response = [Text.Encoding]::ASCII.GetBytes("HTTP/1.1 201 Created`r`nContent-Length: 0`r`nConnection: close`r`n`r`n")
                    $stream.Write($response, 0, $response.Length)
                } finally { $client.Dispose() }
            } finally { $listener.Stop() }
        }
        try {
            Start-Sleep -Milliseconds 300
            $result = Invoke-PendingUploads -Root $root -SessionPath $session -UploadUri ("http://127.0.0.1:{0}/diagnostics" -f $port)
            Wait-Job -Job $job -Timeout 10 | Out-Null
            Receive-Job -Job $job -ErrorAction Stop | Out-Null
        } finally {
            Remove-Job -Job $job -Force -ErrorAction SilentlyContinue
        }

        Assert-Equal 1 $result.Uploaded "successful upload count"
        Assert-Equal 0 $result.Pending "pending count after success"
        Assert-True (-not (Test-Path -LiteralPath $archive.Zip)) "pending ZIP remains"
        Assert-True (Test-Path -LiteralPath (Join-Path (Join-Path $root "sent") ($run.Run + ".zip"))) "sent ZIP missing"
        $request = Get-Content -LiteralPath $capture -Raw
        Assert-True ($request -match "(?im)^POST /diagnostics HTTP/") "wrong request path"
        Assert-True ($request -match "(?im)^Authorization: Bearer fixture-token\s*$") "wrong authorization"
        Assert-True ($request -match "(?im)^Content-Type: application/zip\s*$") "wrong content type"
        Assert-True ($request -match ("(?im)^x-orange-build: " + $run.Build + "\s*$")) "wrong build header"
        Assert-True ($request -match ("(?im)^x-orange-run: " + $run.Run + "\s*$")) "wrong run header"
        Assert-True ($request -match ("(?im)^x-orange-device: " + $run.Device + "\s*$")) "wrong device header"
        Assert-True ($request -match ("(?im)^x-orange-profile: " + $run.Profile + "\s*$")) "wrong profile header"
        Assert-True ($request -notmatch "do-not-send") "unrelated session data was sent"
    }
} finally {
    foreach ($path in $script:TempRoots) {
        Remove-Item -LiteralPath $path -Recurse -Force -ErrorAction SilentlyContinue
    }
}

Write-Host "RESULT: $script:Passed passed, $script:Failed failed"
if ($script:Failed -ne 0) { exit 1 }
