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
    }
}

# Only the Azure boundary is replaced: the real script resolves its own files,
# parses CLI JSON, decides whether to add CORS, and performs ordered uploads.
function az {
    $arguments = [string[]]$args
    $state = $global:OrangeWebsiteTestState
    $state.calls.Add($arguments)
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
try {
    New-Item -ItemType Directory -Path "$fixture\deploy", "$fixture\website\fonts", "$fixture\website\screenshots", "$fixture\assets" -Force | Out-Null
    Copy-Item -LiteralPath $scriptPath -Destination "$fixture\deploy\website.ps1"
    $deploy = "$fixture\deploy\website.ps1"
    $assets = @(
        'website/index.html', 'website/styles.css', 'website/release.js', 'website/404.html', 'assets/logo.png',
        'website/fonts/orbitron-latin-700.woff2', 'website/fonts/ibm-plex-mono-latin-400.woff2',
        'website/fonts/orbitron-OFL.txt', 'website/fonts/ibm-plex-mono-OFL.txt',
        'website/screenshots/home.png', 'website/screenshots/pick.png', 'website/screenshots/streaming.png'
    )
    foreach ($asset in $assets) { Set-Content -LiteralPath (Join-Path $fixture $asset) -Value 'fixture' }
    Set-Content -LiteralPath "$fixture\website\private.txt" -Value 'must never upload'

    # Missing even the final asset must stop before enabling hosting or CORS.
    foreach ($asset in $assets) {
        Reset-Azure
        $path = Join-Path $fixture $asset
        Remove-Item -LiteralPath $path
        $caught = $null
        try { & $deploy | Out-Null } catch { $caught = $_ }
        Assert ($null -ne $caught) "Missing $asset must fail preflight"
        Assert ($global:OrangeWebsiteTestState.calls.Count -eq 0) "Missing $asset must fail before any Azure command"
        Set-Content -LiteralPath $path -Value 'fixture'
    }
    Write-Host 'PASS: All local assets are checked before Azure is changed'

    Reset-Azure
    & $deploy | Out-Null
    $firstCalls = @($global:OrangeWebsiteTestState.calls.ToArray())
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
    }
    $uploads = @($firstCalls | Where-Object { ($_ | Select-Object -First 3) -join ' ' -eq 'storage blob upload' })
    Assert ($uploads.Count -eq 12) 'Only the twelve public assets may be uploaded'
    $names = @()
    foreach ($upload in $uploads) {
        $name = Option $upload '--name'
        Assert ($expected.ContainsKey($name)) "Unexpected public file: $name"
        $names += $name
        Assert ((Option $upload '--container-name') -ceq '$web') 'Uploads must target the literal $web container'
        Assert ((Option $upload '--file') -eq (Join-Path $fixture $expected[$name][0])) "Wrong source for $name"
        Assert ((Option $upload '--content-type') -eq $expected[$name][1]) "Wrong MIME type for $name"
        Assert ((Option $upload '--content-cache-control') -eq 'no-cache') "Mutable $name must be revalidated"
        Assert ((Option $upload '--overwrite') -eq 'true') "Repeat deployment must overwrite $name"
        Assert ((Option $upload '--auth-mode') -eq 'key') 'Uploads must use key authentication'
    }
    Assert (@($names | Select-Object -Unique).Count -eq 12) 'Each public asset must be uploaded once'
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

    & $deploy | Out-Null
    Assert ($global:OrangeWebsiteTestState.cors.Count -eq 1) 'Repeating deployment must not duplicate CORS'
    Write-Host 'PASS: Repeating deployment does not duplicate CORS'

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
        }
    }
    Write-Host 'PASS: Native exits, error JSON, and malformed JSON stop every subsequent mutation'
    Write-Host 'RESULT: Website deployment checks passed'
}
finally {
    $global:LASTEXITCODE = $savedExitCode
    $global:OrangeWebsiteTestState = $savedTestState
    if (Test-Path -LiteralPath $fixture) { Remove-Item -LiteralPath $fixture -Recurse -Force }
}
