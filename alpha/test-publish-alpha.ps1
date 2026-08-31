$ErrorActionPreference = "Stop"

$publisher = Join-Path $PSScriptRoot "publish-alpha.ps1"
if (-not (Test-Path -LiteralPath $publisher -PathType Leaf)) {
    throw "Expected RED: publisher script is absent"
}
. $publisher -LibraryOnly
$launcher = Join-Path $PSScriptRoot "orange-alpha.ps1"
. $launcher

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
    if ($script:AlphaManifestUrl -cne "https://orangealpha0d8d5893e69a3.blob.core.windows.net/releases/orange-alpha.json") {
        throw "launcher does not use the public alpha manifest"
    }
    $manifestJson = $manifest | ConvertTo-Json -Depth 8
    $manifestBytes = [byte[]]([Text.Encoding]::UTF8.GetPreamble() + [Text.Encoding]::UTF8.GetBytes($manifestJson))
    $decodedManifest = ConvertFrom-AlphaWebContent -Content $manifestBytes
    if ($decodedManifest -cne $manifestJson) { throw "byte response was not decoded as UTF-8 without BOM" }
    $legacyDecoded = ([char]0x00EF).ToString() + [char]0x00BB + [char]0x00BF + $manifestJson
    if ((ConvertFrom-AlphaWebContent -Content $legacyDecoded) -cne $manifestJson) { throw "PowerShell 5.1 BOM mojibake was not removed" }

    $probe = Test-NativeCommandSucceeds -Command "cmd.exe" -Arguments @("/d", "/c", "exit 7")
    if ($probe) { throw "nonzero native probe was reported as successful" }

    if ($manifest.schema -ne 1) { throw "wrong schema" }
    if ($manifest.build -cne $build) { throw "wrong build" }
    if ($manifest.sha256 -cne $hash.ToUpperInvariant()) { throw "wrong hash" }
    if ($manifest.asset_url -cne "https://orangealpha0d8d5893e69a3.blob.core.windows.net/releases/orange-alpha-0123456.zip") { throw "wrong asset URL" }

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
