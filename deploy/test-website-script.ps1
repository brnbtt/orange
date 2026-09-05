$ErrorActionPreference = "Stop"

function Assert($condition, [string]$message) {
    if (-not $condition) { throw $message }
}

function Option([string[]]$arguments, [string]$name) {
    $index = [Array]::IndexOf($arguments, $name)
    if ($index -lt 0) { return $null }
    return $arguments[$index + 1]
}

function Reset-Azure {
    $global:OrangeWebsiteTestState = @{
        calls = New-Object System.Collections.Generic.List[object]
        cors = @()
        failAt = 0
        failureMode = "exit"
        endpoint = "https://fixture.z99.web.core.windows.net/"
        requests = New-Object System.Collections.Generic.List[object]
        manifest = ('{"schema":1,"channel":"beta","version":"1.0.0","installer_url":"https://orangealpha0d8d5893e69a3.blob.core.windows.net/releases/orange-setup-1.0.0.exe"}' | ConvertFrom-Json)
        httpError = $false
        uploadedIndex = $null
        stagedPath = $null
    }
}

# Public HTTP and Azure are the only replaced boundaries; fixture files and
# staging stay real so stale fallbacks and leaked staging files are observable.
function Invoke-RestMethod {
    param([string]$Uri, [string]$Method, [int]$TimeoutSec)
    $state = $global:OrangeWebsiteTestState
    $state.requests.Add(@{ Uri = $Uri; Method = $Method; TimeoutSec = $TimeoutSec; AzureCalls = $state.calls.Count })
    if ($state.httpError) { throw 'Fixture public manifest request failed' }
    return $state.manifest
}

function az {
    $arguments = [string[]]$args
    $state = $global:OrangeWebsiteTestState
    $state.calls.Add($arguments)
    if ((Option $arguments '--name') -eq 'index.html') {
        $state.stagedPath = Option $arguments '--file'
        $state.uploadedIndex = [IO.File]::ReadAllText($state.stagedPath)
    }
    $global:LASTEXITCODE = 0
    if ($state.failAt -eq $state.calls.Count) {
        if ($state.failureMode -like "exit*") { $global:LASTEXITCODE = 23 }
        if ($state.failureMode -eq "exit") { return '{"ok":true}' }
        if ($state.failureMode -eq "malformed") { return 'not JSON' }
        return '{"error":{"code":"FixtureFailure","message":"fixture Azure failure"}}'
    }
    $command = ($arguments | Select-Object -First 4) -join ' '
    if ($command -like 'storage account show *') {
        return (@{ primaryEndpoints = @{ web = $state.endpoint } } | ConvertTo-Json -Depth 5)
    }
    if ($command -eq 'storage blob service-properties show') {
        return (@{ cors = @($state.cors); staticWebsite = @{ enabled = $false } } | ConvertTo-Json -Depth 8)
    }
    if ($command -eq 'storage blob service-properties update') { return }
    if ($command -like 'storage cors add *') {
        Assert (-not ($arguments -contains '--auth-mode')) 'cors add does not support --auth-mode'
        $state.cors += @{ allowedOrigins = @((Option $arguments '--origins')); allowedMethods = @('GET', 'HEAD'); allowedHeaders = @(); exposedHeaders = @(); maxAgeInSeconds = 3600 }
        return
    }
    if ($command -like 'storage blob upload *') { return '{"etag":"fixture"}' }
    throw "Unexpected Azure command: $($arguments -join ' ')"
}

$scriptPath = Join-Path $PSScriptRoot 'website.ps1'
Assert (Test-Path -LiteralPath $scriptPath -PathType Leaf) 'Website deployment script must exist'
$fixture = Join-Path $env:LOCALAPPDATA "Temp\opencode\website-test-$([Guid]::NewGuid().ToString('N'))"
$savedExitCode = $global:LASTEXITCODE
$savedTestState = $global:OrangeWebsiteTestState
$savedTemp = $env:TEMP
$savedTmp = $env:TMP
try {
    New-Item -ItemType Directory -Path "$fixture\deploy", "$fixture\website\fonts", "$fixture\website\screenshots", "$fixture\assets", "$fixture\temp" -Force | Out-Null
    $env:TEMP = "$fixture\temp"
    $env:TMP = "$fixture\temp"
    Copy-Item -LiteralPath $scriptPath -Destination "$fixture\deploy\website.ps1"
    $deploy = "$fixture\deploy\website.ps1"
    $assets = @(
        'website/index.html', 'website/styles.css', 'website/release.js', 'website/404.html', 'assets/logo.png',
        'website/fonts/orbitron-latin-700.woff2', 'website/fonts/ibm-plex-mono-latin-400.woff2',
        'website/fonts/orbitron-OFL.txt', 'website/fonts/ibm-plex-mono-OFL.txt',
        'website/screenshots/home.png', 'website/screenshots/pick.png', 'website/screenshots/streaming.png',
        'website/screenshots/add-friend.png', 'website/screenshots/requests.png', 'website/screenshots/requests-incoming.png'
    )
    foreach ($asset in $assets) { Set-Content -LiteralPath (Join-Path $fixture $asset) -Value 'fixture' }
    $sourceIndex = @'
<!doctype html>
<a href="https://orangealpha0d8d5893e69a3.blob.core.windows.net/releases/orange-setup-0.9.2.exe" data-download>Download for Windows</a>
<a data-download href="https://orangealpha0d8d5893e69a3.blob.core.windows.net/releases/orange-setup-0.9.2.exe">Download for Windows</a>
<noscript>Snapshot version <span data-fallback-version>0.9.2</span></noscript>
<p>Keep unrelated version 0.9.2 and https://example.com/orange-setup-0.9.2.exe intact.</p>
'@
    $indexPath = "$fixture\website\index.html"
    [IO.File]::WriteAllText($indexPath, $sourceIndex)
    Set-Content -LiteralPath "$fixture\website\private.txt" -Value 'must never upload'

    # Missing even the final asset must stop before enabling hosting or CORS.
    foreach ($asset in $assets) {
        Reset-Azure
        $path = Join-Path $fixture $asset
        $originalBytes = [IO.File]::ReadAllBytes($path)
        Remove-Item -LiteralPath $path
        $caught = $null
        try { & $deploy | Out-Null } catch { $caught = $_ }
        Assert ($null -ne $caught) "Missing $asset must fail preflight"
        Assert ($global:OrangeWebsiteTestState.calls.Count -eq 0) "Missing $asset must fail before any Azure command"
        Assert ($global:OrangeWebsiteTestState.requests.Count -eq 0) "Missing $asset must fail before fetching the manifest"
        [IO.File]::WriteAllBytes($path, $originalBytes)
    }
    Write-Host 'PASS: All local assets are checked before Azure is changed'

    # The private GitHub release returned anonymous 404. An unavailable or
    # untrusted public snapshot must not replace a working deployed download.
    foreach ($case in @(
        @{ field = 'schema'; value = 2 }, @{ field = 'schema'; value = '1' },
        @{ field = 'schema'; value = $true }, @{ field = 'schema'; value = $null },
        @{ field = 'channel'; value = 'stable' }, @{ field = 'channel'; value = 'BETA' },
        @{ field = 'version'; value = '1.0.0-beta.1' }, @{ field = 'version'; value = '01.0.0' },
        @{ field = 'version'; value = "1.0.0`n" }, @{ field = 'version'; value = 1 },
        @{ field = 'installer_url'; value = 'https://example.com/orange-setup-1.0.0.exe' },
        @{ field = 'installer_url'; value = 'https://orangealpha0d8d5893e69a3.blob.core.windows.net/releases/orange-setup-0.9.2.exe' },
        @{ field = 'installer_url'; value = 'https://orangealpha0d8d5893e69a3.blob.core.windows.net/releases/orange-setup-1.0.0.exe?token=unexpected' },
        @{ field = 'installer_url'; value = 'http://orangealpha0d8d5893e69a3.blob.core.windows.net/releases/orange-setup-1.0.0.exe' },
        @{ field = 'installer_url'; value = 'https://orangealpha0d8d5893e69a3.blob.core.windows.net/releases/ORANGE-SETUP-1.0.0.exe' },
        @{ field = 'empty' }, @{ field = 'http-error' }
    )) {
        Reset-Azure
        if ($case.field -eq 'empty') { $global:OrangeWebsiteTestState.manifest = $null }
        elseif ($case.field -eq 'http-error') { $global:OrangeWebsiteTestState.httpError = $true }
        else { $global:OrangeWebsiteTestState.manifest.($case.field) = $case.value }
        $caught = $null
        try { & $deploy | Out-Null } catch { $caught = $_ }
        Assert ($null -ne $caught) "Invalid/unavailable manifest ($($case.field)) must fail deployment"
        Assert ($global:OrangeWebsiteTestState.calls.Count -eq 0) 'Invalid/unavailable manifest must fail before Azure mutation'
        Assert ([IO.File]::ReadAllText($indexPath) -ceq $sourceIndex) 'Manifest failure must leave source HTML untouched'
        Assert (@(Get-ChildItem -LiteralPath "$fixture\temp" -Force).Count -eq 0) 'Manifest failure must leave no staging files'
    }
    Write-Host 'PASS: Unavailable or invalid public manifests fail before Azure is changed'

    foreach ($html in @(
        $sourceIndex.Replace('<span data-fallback-version>0.9.2</span>', ''),
        ($sourceIndex + '<span data-fallback-version>0.9.2</span>'),
        $sourceIndex.Replace('data-download href=', 'data-download data-old-href='),
        ($sourceIndex + '<a href="https://orangealpha0d8d5893e69a3.blob.core.windows.net/releases/orange-setup-0.9.2.exe">Extra</a>')
    )) {
        Reset-Azure
        [IO.File]::WriteAllText($indexPath, $html)
        $caught = $null
        try { & $deploy | Out-Null } catch { $caught = $_ }
        Assert ($null -ne $caught) 'An ambiguous or missing HTML replacement target must fail deployment'
        Assert ($global:OrangeWebsiteTestState.calls.Count -eq 0) 'HTML snapshot validation must precede Azure mutation'
        Assert ([IO.File]::ReadAllText($indexPath) -ceq $html) 'HTML validation must leave its source untouched'
        Assert (@(Get-ChildItem -LiteralPath "$fixture\temp" -Force).Count -eq 0) 'HTML validation failure must leave no staging files'
    }
    [IO.File]::WriteAllText($indexPath, $sourceIndex)
    Write-Host 'PASS: Exactly two trusted href targets and one fallback version marker are required'

    Reset-Azure
    & $deploy | Out-Null
    $firstCalls = @($global:OrangeWebsiteTestState.calls.ToArray())
    $requests = $global:OrangeWebsiteTestState.requests
    Assert ($requests.Count -eq 1) 'Every deployment must fetch one current public manifest'
    Assert ($requests[0].AzureCalls -eq 0) 'The public manifest must be fetched before any Azure command'
    Assert ($requests[0].Uri -ceq 'https://orangealpha0d8d5893e69a3.blob.core.windows.net/releases/orange-beta.json') 'Manifest must come from the fixed public release URL'
    Assert ($requests[0].Method -eq 'Get' -and $requests[0].TimeoutSec -eq 20) 'Public manifest GET must time out after twenty seconds'
    $expectedIndex = $sourceIndex.Replace('https://orangealpha0d8d5893e69a3.blob.core.windows.net/releases/orange-setup-0.9.2.exe', 'https://orangealpha0d8d5893e69a3.blob.core.windows.net/releases/orange-setup-1.0.0.exe').Replace('<span data-fallback-version>0.9.2</span>', '<span data-fallback-version>1.0.0</span>')
    Assert ($global:OrangeWebsiteTestState.uploadedIndex -ceq $expectedIndex) 'Uploaded HTML must snapshot the current public installer and matching fallback version only'
    Assert ([IO.File]::ReadAllText($indexPath) -ceq $sourceIndex) 'Successful deployment must leave source HTML untouched'
    Assert (-not (Test-Path -LiteralPath $global:OrangeWebsiteTestState.stagedPath)) 'Successful deployment must clean up its staged index'
    Assert (@(Get-ChildItem -LiteralPath "$fixture\temp" -Force).Count -eq 0) 'Successful deployment must leave no staging files'
    Write-Host 'PASS: Deployment uploads a current public download snapshot without changing source HTML or leaving staging files'
    $enable = @($firstCalls | Where-Object { ($_ | Select-Object -First 4) -join ' ' -eq 'storage blob service-properties update' })
    Assert ($enable.Count -eq 1) 'Static hosting must be enabled once per deployment'
    Assert ((Option $enable[0] '--static-website') -eq 'true') 'Static hosting must be enabled'
    Assert ((Option $enable[0] '--index-document') -eq 'index.html') 'Index document must be index.html'
    Assert ((Option $enable[0] '--404-document') -eq '404.html') 'Error document must be 404.html'
    $add = @($firstCalls | Where-Object { ($_ | Select-Object -First 3) -join ' ' -eq 'storage cors add' })
    Assert ($add.Count -eq 1) 'Empty CORS must receive one rule'
    Assert ((Option $add[0] '--origins') -eq 'https://fixture.z99.web.core.windows.net') 'CORS must use the discovered origin without its trailing slash'
    Assert ((Option $add[0] '--services') -eq 'b') 'CORS must target only blob service'
    Assert ((Option $add[0] '--max-age') -eq '3600') 'CORS max age must be 3600'
    $methodIndex = [Array]::IndexOf($add[0], '--methods')
    Assert (($add[0][$methodIndex + 1] -eq 'GET') -and ($add[0][$methodIndex + 2] -eq 'HEAD')) 'CORS must allow GET and HEAD'

    $expected = @{
        'index.html' = @('website/index.html', 'text/html; charset=utf-8')
        'styles.css' = @('website/styles.css', 'text/css; charset=utf-8')
        'release.js' = @('website/release.js', 'application/javascript; charset=utf-8')
        '404.html' = @('website/404.html', 'text/html; charset=utf-8')
        'logo.png' = @('assets/logo.png', 'image/png')
        'fonts/orbitron-latin-700.woff2' = @('website/fonts/orbitron-latin-700.woff2', 'font/woff2')
        'fonts/ibm-plex-mono-latin-400.woff2' = @('website/fonts/ibm-plex-mono-latin-400.woff2', 'font/woff2')
        'fonts/orbitron-OFL.txt' = @('website/fonts/orbitron-OFL.txt', 'text/plain; charset=utf-8')
        'fonts/ibm-plex-mono-OFL.txt' = @('website/fonts/ibm-plex-mono-OFL.txt', 'text/plain; charset=utf-8')
        'screenshots/home.png' = @('website/screenshots/home.png', 'image/png')
        'screenshots/pick.png' = @('website/screenshots/pick.png', 'image/png')
        'screenshots/streaming.png' = @('website/screenshots/streaming.png', 'image/png')
        'screenshots/add-friend.png' = @('website/screenshots/add-friend.png', 'image/png')
        'screenshots/requests.png' = @('website/screenshots/requests.png', 'image/png')
        'screenshots/requests-incoming.png' = @('website/screenshots/requests-incoming.png', 'image/png')
    }
    $uploads = @($firstCalls | Where-Object { ($_ | Select-Object -First 3) -join ' ' -eq 'storage blob upload' })
    Assert ($uploads.Count -eq 15) 'Only the fifteen public assets may be uploaded'
    $names = @()
    foreach ($upload in $uploads) {
        $name = Option $upload '--name'
        Assert ($expected.ContainsKey($name)) "Unexpected public file: $name"
        $names += $name
        Assert ((Option $upload '--container-name') -ceq '$web') 'Uploads must target the literal $web container'
        if ($name -eq 'index.html') {
            Assert ((Option $upload '--file') -ne $indexPath) 'Index upload must use a staged file'
        } else {
            Assert ((Option $upload '--file') -eq (Join-Path $fixture $expected[$name][0])) "Wrong source for $name"
        }
        Assert ((Option $upload '--content-type') -eq $expected[$name][1]) "Wrong MIME type for $name"
        Assert ((Option $upload '--content-cache-control') -eq 'no-cache') "Mutable $name must be revalidated"
        Assert ((Option $upload '--overwrite') -eq 'true') "Repeat deployment must overwrite $name"
        Assert ((Option $upload '--auth-mode') -eq 'key') 'Uploads must use key authentication'
    }
    Assert (@($names | Select-Object -Unique).Count -eq 15) 'Each public asset must be uploaded once'
    Assert ($names[-1] -eq 'index.html') 'Index must be uploaded after its dependencies'
    foreach ($call in $firstCalls) {
        if ($call[1] -eq 'account') {
            Assert ((Option $call '--name') -eq 'orangealpha0d8d5893e69a3') 'Account lookup must use the existing release account'
            Assert ((Option $call '--resource-group') -eq 'orange-rg') 'Account lookup must use the existing resource group'
        } else {
            Assert ((Option $call '--account-name') -eq 'orangealpha0d8d5893e69a3') 'All mutations must use the existing release account'
        }
    }
    Write-Host 'PASS: Deployment enables hosting and uploads only the public assets with correct headers and index last'

    $global:OrangeWebsiteTestState.manifest.version = '1.0.1'
    $global:OrangeWebsiteTestState.manifest.installer_url = 'https://orangealpha0d8d5893e69a3.blob.core.windows.net/releases/orange-setup-1.0.1.exe'
    & $deploy | Out-Null
    Assert ($global:OrangeWebsiteTestState.cors.Count -eq 1) 'Repeating deployment must not duplicate CORS'
    Assert ($global:OrangeWebsiteTestState.requests.Count -eq 2) 'Repeating deployment must fetch a fresh manifest'
    Assert ($global:OrangeWebsiteTestState.requests[1].AzureCalls -eq $firstCalls.Count) 'Repeated deployment must fetch before its first Azure command'
    Assert ($global:OrangeWebsiteTestState.uploadedIndex -ceq $expectedIndex.Replace('1.0.0', '1.0.1')) 'Repeated deployment must snapshot the newly published version'
    Write-Host 'PASS: Repeating deployment refreshes the snapshot without duplicating CORS'

    # A shared account may already serve other origins. Replacing its rule list
    # or testing an origin without its methods would break those consumers.
    foreach ($case in @(
        @{ origins = @('https://unrelated.example'); methods = @('GET'); count = 2 },
        @{ origins = @('https://fixture.z99.web.core.windows.net'); methods = @('HEAD'); count = 2 },
        @{ origins = @('https://unrelated.example', 'https://fixture.z99.web.core.windows.net'); methods = @('POST', 'GET'); count = 1 },
        @{ origins = 'https://unrelated.example,https://fixture.z99.web.core.windows.net'; methods = 'POST,GET'; count = 1 },
        @{ origins = @('*'); methods = @('GET'); count = 1 },
        @{ origins = @('https://*.z99.web.core.windows.net'); methods = @('GET'); count = 1 }
    )) {
        Reset-Azure
        $global:OrangeWebsiteTestState.cors = @(@{ allowedOrigins = $case.origins; allowedMethods = $case.methods; allowedHeaders = @('x-existing'); exposedHeaders = @('etag'); maxAgeInSeconds = 42 })
        $original = $global:OrangeWebsiteTestState.cors[0] | ConvertTo-Json -Depth 5 -Compress
        & $deploy | Out-Null
        Assert ($global:OrangeWebsiteTestState.cors.Count -eq $case.count) 'CORS must be added only when no rule allows this origin and GET'
        Assert (($global:OrangeWebsiteTestState.cors[0] | ConvertTo-Json -Depth 5 -Compress) -eq $original) 'Existing CORS rules must be preserved'
    }
    Write-Host 'PASS: Existing exact and wildcard CORS rules are respected and unrelated rules are preserved'

    # PowerShell does not turn native nonzero exits into terminating errors.
    # Exercise every CLI boundary, including mutations that return error JSON.
    foreach ($mode in @('exit', 'exit-json', 'json', 'malformed')) {
        for ($i = 1; $i -le $firstCalls.Count; $i++) {
            Reset-Azure
            $global:OrangeWebsiteTestState.failAt = $i
            $global:OrangeWebsiteTestState.failureMode = $mode
            $caught = $null
            try { & $deploy | Out-Null } catch { $caught = $_ }
            Assert ($null -ne $caught) "Azure $mode failure at command $i must fail deployment"
            Assert ($global:OrangeWebsiteTestState.calls.Count -eq $i) "Azure $mode failure at command $i must prevent subsequent commands"
            Assert ([IO.File]::ReadAllText($indexPath) -ceq $sourceIndex) 'Azure failure must leave source HTML untouched'
            Assert (@(Get-ChildItem -LiteralPath "$fixture\temp" -Force).Count -eq 0) "Azure $mode failure at command $i must clean up its staged index"
        }
    }
    Write-Host 'PASS: Native exits, error JSON, and malformed JSON stop every subsequent mutation'
    Write-Host 'RESULT: Website deployment checks passed'
}
finally {
    $env:TEMP = $savedTemp
    $env:TMP = $savedTmp
    $global:LASTEXITCODE = $savedExitCode
    $global:OrangeWebsiteTestState = $savedTestState
    if (Test-Path -LiteralPath $fixture) { Remove-Item -LiteralPath $fixture -Recurse -Force }
}
