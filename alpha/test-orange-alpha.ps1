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
        Assert-True ($request -notmatch "do-not-send") "unrelated session data was sent"
    }
} finally {
    foreach ($path in $script:TempRoots) {
        Remove-Item -LiteralPath $path -Recurse -Force -ErrorAction SilentlyContinue
    }
}

Write-Host "RESULT: $script:Passed passed, $script:Failed failed"
if ($script:Failed -ne 0) { exit 1 }
