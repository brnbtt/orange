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

    $source = Get-Content -LiteralPath $publisher -Raw
    $installerUpload = $source.IndexOf('"--name", $installerName')
    $manifestUpload = $source.IndexOf('"--name", "orange-beta.json"')
    if ($installerUpload -lt 0 -or $manifestUpload -le $installerUpload) {
        throw "publisher does not upload the installer before the manifest"
    }
    Write-Host "RESULT: beta publisher checks passed"
} finally {
    Remove-Item -LiteralPath $root -Recurse -Force -ErrorAction SilentlyContinue
}
