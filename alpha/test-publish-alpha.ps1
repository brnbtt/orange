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
$zip = Join-Path $root "orange-alpha-0123456789abcdef0123456789abcdef01234567.zip"
New-Item -ItemType Directory -Path $stage -Force | Out-Null
Set-Content -LiteralPath (Join-Path $stage "orange.exe") -Value "media" -Encoding ASCII
Set-Content -LiteralPath (Join-Path $stage "orange-tray.exe") -Value "tray" -Encoding ASCII
Set-Content -LiteralPath (Join-Path $stage "README.txt") -Value "alpha" -Encoding ASCII
Compress-Archive -Path (Join-Path $stage "*") -DestinationPath $zip

try {
    $build = "0123456789abcdef0123456789abcdef01234567"
    $hash = (Get-FileHash -LiteralPath $zip -Algorithm SHA256).Hash
    $manifest = New-AlphaManifest -Build $build -AssetName "orange-alpha-$build.zip" -Sha256 $hash -Profile "hardware-bounded-jitter" -Environment ([ordered]@{
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
    if ($manifest.asset_url -cne "https://orangealpha0d8d5893e69a3.blob.core.windows.net/releases/orange-alpha-$build.zip") { throw "wrong asset URL" }

    $script:ShimCommands = New-Object System.Collections.Generic.List[object]
    $script:GitHubAssets = @()
    $script:AzureBlob = $null
    $script:RemoteArchive = $zip
    function Get-OptionalNativeJson {
        param([string]$Command, [string[]]$Arguments)
        $script:ShimCommands.Add([pscustomobject]@{ Command = $Command; Arguments = $Arguments })
        if ($Command -eq "gh") {
            return [pscustomobject]@{ assets = $script:GitHubAssets }
        }
        return $script:AzureBlob
    }
    function Invoke-CheckedCommand {
        param([string]$Command, [string[]]$Arguments)
        $script:ShimCommands.Add([pscustomobject]@{ Command = $Command; Arguments = $Arguments })
        if ($Command -eq "gh" -and $Arguments[0] -eq "release" -and $Arguments[1] -eq "download") {
            $directory = $Arguments[[array]::IndexOf($Arguments, "--dir") + 1]
            Copy-Item -LiteralPath $script:RemoteArchive -Destination (Join-Path $directory (Split-Path -Leaf $zip))
        }
        if ($Command -eq "az" -and $Arguments[0] -eq "storage" -and $Arguments[2] -eq "download") {
            $destination = $Arguments[[array]::IndexOf($Arguments, "--file") + 1]
            Copy-Item -LiteralPath $script:RemoteArchive -Destination $destination
        }
    }

    Ensure-GitHubAlphaArchive -Archive $zip -AssetName (Split-Path -Leaf $zip)
    $githubUpload = @($script:ShimCommands | Where-Object {
        $_.Command -eq "gh" -and $_.Arguments[0] -eq "release" -and $_.Arguments[1] -eq "upload"
    })
    if ($githubUpload.Count -ne 1 -or $githubUpload[0].Arguments -contains "--clobber") {
        throw "GitHub alpha archive is not create-only"
    }

    $script:ShimCommands.Clear()
    Ensure-AzureAlphaArchive -Archive $zip -AssetName (Split-Path -Leaf $zip) -Build $build -Sha256 $hash
    $azureUpload = @($script:ShimCommands | Where-Object {
        $_.Command -eq "az" -and $_.Arguments[0] -eq "storage" -and $_.Arguments[2] -eq "upload"
    })
    $overwrite = [array]::IndexOf($azureUpload[0].Arguments, "--overwrite")
    if ($azureUpload.Count -ne 1 -or $overwrite -lt 0 -or $azureUpload[0].Arguments[$overwrite + 1] -cne "false") {
        throw "Azure alpha archive is not create-only"
    }

    $script:ShimCommands.Clear()
    $script:GitHubAssets = @([pscustomobject]@{ name = (Split-Path -Leaf $zip); size = (Get-Item $zip).Length })
    $script:AzureBlob = [pscustomobject]@{
        properties = [pscustomobject]@{ contentLength = (Get-Item $zip).Length }
        metadata = [pscustomobject]@{ sha256 = $hash; build = $build }
    }
    Ensure-GitHubAlphaArchive -Archive $zip -AssetName (Split-Path -Leaf $zip)
    Ensure-AzureAlphaArchive -Archive $zip -AssetName (Split-Path -Leaf $zip) -Build $build -Sha256 $hash
    if (@($script:ShimCommands | Where-Object { $_.Arguments -contains "upload" }).Count -ne 0) {
        throw "matching immutable alpha archives were uploaded again"
    }

    $badRemote = Join-Path $root "bad-remote.zip"
    $badBytes = [IO.File]::ReadAllBytes($zip)
    $badBytes[0] = $badBytes[0] -bxor 1
    [IO.File]::WriteAllBytes($badRemote, $badBytes)
    $script:RemoteArchive = $badRemote
    foreach ($verify in @("GitHub", "Azure")) {
        try {
            if ($verify -eq "GitHub") {
                Ensure-GitHubAlphaArchive -Archive $zip -AssetName (Split-Path -Leaf $zip)
            } else {
                Ensure-AzureAlphaArchive -Archive $zip -AssetName (Split-Path -Leaf $zip) -Build $build -Sha256 $hash
            }
            throw "$verify accepted altered remote alpha bytes"
        } catch {
            if ($_.Exception.Message -eq "$verify accepted altered remote alpha bytes") { throw }
        }
    }
    $script:RemoteArchive = $zip

    $script:GitHubAssets[0].size++
    try {
        Ensure-GitHubAlphaArchive -Archive $zip -AssetName (Split-Path -Leaf $zip)
        throw "conflicting GitHub alpha archive was accepted"
    } catch {
        if ($_.Exception.Message -eq "conflicting GitHub alpha archive was accepted") { throw }
    }
    $script:GitHubAssets[0].size--
    $script:AzureBlob.metadata.sha256 = "BAD"
    try {
        Ensure-AzureAlphaArchive -Archive $zip -AssetName (Split-Path -Leaf $zip) -Build $build -Sha256 $hash
        throw "conflicting Azure alpha archive was accepted"
    } catch {
        if ($_.Exception.Message -eq "conflicting Azure alpha archive was accepted") { throw }
    }

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
