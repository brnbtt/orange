$ErrorActionPreference = "Stop"

$publisher = Join-Path $PSScriptRoot "publish-alpha.ps1"
if (-not (Test-Path -LiteralPath $publisher -PathType Leaf)) {
    throw "Expected RED: publisher script is absent"
}
. $publisher -LibraryOnly

$root = Join-Path $env:TEMP ("orange-alpha-publish-test-" + [guid]::NewGuid().ToString("N"))
$stage = Join-Path $root "stage"
$zip = Join-Path $root "orange-alpha-deadbee.zip"
New-Item -ItemType Directory -Path $stage -Force | Out-Null
Set-Content -LiteralPath (Join-Path $stage "orange.exe") -Value "media" -Encoding ASCII
Set-Content -LiteralPath (Join-Path $stage "orange-tray.exe") -Value "tray" -Encoding ASCII
Set-Content -LiteralPath (Join-Path $stage "README.txt") -Value "alpha" -Encoding ASCII
Compress-Archive -Path (Join-Path $stage "*") -DestinationPath $zip

try {
    $build = "0123456789abcdef0123456789abcdef01234567"
    $hash = (Get-FileHash -LiteralPath $zip -Algorithm SHA256).Hash
    $manifest = New-AlphaManifest -Build $build -AssetName "orange-alpha-0123456.zip" -Sha256 $hash -Profile "hardware-bounded-jitter" -Environment ([ordered]@{
        ORANGE_AV1_DECODER = "hardware"
        ORANGE_RTP_BUFFER_MODE = ""
    })

    Test-AlphaPublishManifest -Manifest $manifest | Out-Null
    Test-AlphaPublishArchive -Path $zip | Out-Null

    $probe = Test-NativeCommandSucceeds -Command "cmd.exe" -Arguments @("/d", "/c", "exit 7")
    if ($probe) { throw "nonzero native probe was reported as successful" }

    if ($manifest.schema -ne 1) { throw "wrong schema" }
    if ($manifest.build -cne $build) { throw "wrong build" }
    if ($manifest.sha256 -cne $hash.ToUpperInvariant()) { throw "wrong hash" }
    if ($manifest.asset_url -cne "https://github.com/brnbtt/orange/releases/download/alpha-latest/orange-alpha-0123456.zip") { throw "wrong asset URL" }

    $bad = $manifest | ConvertTo-Json -Depth 8 | ConvertFrom-Json
    $bad.sha256 = "bad"
    try {
        Test-AlphaPublishManifest -Manifest $bad | Out-Null
        throw "invalid manifest was accepted"
    } catch {
        if ($_.Exception.Message -eq "invalid manifest was accepted") { throw }
    }

    $badStage = Join-Path $root "bad-stage"
    $badZip = Join-Path $root "bad.zip"
    New-Item -ItemType Directory -Path $badStage | Out-Null
    Set-Content -LiteralPath (Join-Path $badStage "orange.exe") -Value "media" -Encoding ASCII
    Compress-Archive -Path (Join-Path $badStage "*") -DestinationPath $badZip
    try {
        Test-AlphaPublishArchive -Path $badZip | Out-Null
        throw "invalid archive was accepted"
    } catch {
        if ($_.Exception.Message -eq "invalid archive was accepted") { throw }
    }

    Write-Host "RESULT: publisher validation passed"
} finally {
    Remove-Item -LiteralPath $root -Recurse -Force -ErrorAction SilentlyContinue
}
