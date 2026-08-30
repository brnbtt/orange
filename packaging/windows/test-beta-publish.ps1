$ErrorActionPreference = "Stop"
$publisher = Join-Path (Split-Path $PSScriptRoot -Parent | Split-Path -Parent) "publish-beta.ps1"
if (-not (Test-Path -LiteralPath $publisher -PathType Leaf)) {
    throw "Expected RED: beta publisher is absent"
}
. $publisher -LibraryOnly

$root = Join-Path $env:TEMP ("orange-beta-publish-test-" + [guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Path $root | Out-Null
try {
    $installer = Join-Path $root "orange-setup-0.2.0-beta.1.exe"
    [IO.File]::WriteAllBytes($installer, [Text.Encoding]::UTF8.GetBytes("installer"))
    $manifest = New-BetaManifest `
        -Version "0.2.0-beta.1" `
        -Build "0123456789abcdef0123456789abcdef01234567" `
        -InstallerPath $installer `
        -Notes "First beta"
    Test-BetaManifest -Manifest $manifest -InstallerPath $installer | Out-Null
    if ($manifest.installer_url -cne "https://orangealpha0d8d5893e69a3.blob.core.windows.net/releases/orange-setup-0.2.0-beta.1.exe") {
        throw "wrong installer URL"
    }

    $bad = $manifest | ConvertTo-Json | ConvertFrom-Json
    $bad.channel = "stable"
    try {
        Test-BetaManifest -Manifest $bad -InstallerPath $installer | Out-Null
        throw "wrong channel was accepted"
    } catch {
        if ($_.Exception.Message -eq "wrong channel was accepted") { throw }
    }

    $badNotes = $manifest | ConvertTo-Json | ConvertFrom-Json
    $badNotes.notes = "bad`tcontrol"
    try {
        Test-BetaManifest -Manifest $badNotes -InstallerPath $installer | Out-Null
        throw "control character was accepted"
    } catch {
        if ($_.Exception.Message -eq "control character was accepted") { throw }
    }
    $unicodeNotes = $manifest | ConvertTo-Json | ConvertFrom-Json
    $unicodeNotes.notes = ([string][char]0x00E9) * 500
    try {
        Test-BetaManifest -Manifest $unicodeNotes -InstallerPath $installer | Out-Null
        throw "UTF-8 oversized notes were accepted"
    } catch {
        if ($_.Exception.Message -eq "UTF-8 oversized notes were accepted") { throw }
    }

    $oversized = Join-Path $root "orange-setup-0.2.0-beta.1-oversized.exe"
    $stream = [IO.File]::Create($oversized)
    try { $stream.SetLength((250MB) + 1) } finally { $stream.Dispose() }
    Copy-Item -LiteralPath $oversized -Destination $installer -Force
    $oversizedManifest = $manifest | ConvertTo-Json | ConvertFrom-Json
    try {
        Test-BetaManifest -Manifest $oversizedManifest -InstallerPath $installer | Out-Null
        throw "oversized installer was accepted"
    } catch {
        if ($_.Exception.Message -eq "oversized installer was accepted") { throw }
    }

    $source = Get-Content -LiteralPath $publisher -Raw
    $installerUpload = $source.IndexOf('Ensure-AzureInstaller -Installer $installer')
    $manifestUpload = $source.IndexOf('"--name", "orange-beta.json"')
    if ($installerUpload -lt 0 -or $manifestUpload -le $installerUpload) {
        throw "publisher does not upload the installer before the manifest"
    }
    foreach ($required in @('-Command "git" -Arguments @("fetch", "origin", "main")', '-BuildId $build', 'Ensure-GitHubRelease', 'Source changed while building')) {
        if ($source.IndexOf($required) -lt 0) { throw "publisher is missing safety gate: $required" }
    }
    Write-Host "RESULT: beta publisher checks passed"
} finally {
    Remove-Item -LiteralPath $root -Recurse -Force -ErrorAction SilentlyContinue
}
