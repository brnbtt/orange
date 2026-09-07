# Publishes the website through static hosting on the existing release storage
# account. Storage serves the files directly, with no additional compute tier.
#
#   az login
#   .\deploy\website.ps1
#
# Re-running overwrites the public assets and adds CORS only if needed.
# The index is uploaded last so its dependencies exist before it becomes live.
# Each deploy snapshots the public beta download into a temporary index, leaving
# the checked-in preview fallback untouched.

$ErrorActionPreference = "Stop"
$root = Split-Path $PSScriptRoot -Parent
$storageAccount = 'orangealpha0d8d5893e69a3'
$resourceGroup = 'orange-rg'

function Invoke-WebsiteAzure([string[]]$Arguments) {
    # Check the native exit before parsing: Azure can return an error object
    # that is valid JSON, and PowerShell does not throw for a native failure.
    $output = & az @Arguments --only-show-errors --output json
    if ($LASTEXITCODE -ne 0) {
        throw "Azure command failed (exit $LASTEXITCODE): az $($Arguments -join ' ')"
    }
    $text = $output -join "`n"
    if ([string]::IsNullOrWhiteSpace($text)) { return $null }
    $result = $text | ConvertFrom-Json
    if ($null -ne $result.error) {
        throw "Azure command returned an error: $($result.error | ConvertTo-Json -Compress -Depth 8)"
    }
    return $result
}

# A directory upload could expose future drafts or local files. Keep the public
# surface explicit, and validate every source before making any Azure changes.
$uploads = @(
    @{ Source = 'website/styles.css'; Name = 'styles.css'; Type = 'text/css; charset=utf-8' }
    @{ Source = 'website/i18n.js'; Name = 'i18n.js'; Type = 'application/javascript; charset=utf-8' }
    @{ Source = 'website/release.js'; Name = 'release.js'; Type = 'application/javascript; charset=utf-8' }
    @{ Source = 'website/fonts/orbitron-latin-700.woff2'; Name = 'fonts/orbitron-latin-700.woff2'; Type = 'font/woff2' }
    @{ Source = 'website/fonts/ibm-plex-mono-latin-400.woff2'; Name = 'fonts/ibm-plex-mono-latin-400.woff2'; Type = 'font/woff2' }
    @{ Source = 'website/fonts/orbitron-OFL.txt'; Name = 'fonts/orbitron-OFL.txt'; Type = 'text/plain; charset=utf-8' }
    @{ Source = 'website/fonts/ibm-plex-mono-OFL.txt'; Name = 'fonts/ibm-plex-mono-OFL.txt'; Type = 'text/plain; charset=utf-8' }
    @{ Source = 'website/screenshots/home.png'; Name = 'screenshots/home.png'; Type = 'image/png' }
    @{ Source = 'website/screenshots/pick.png'; Name = 'screenshots/pick.png'; Type = 'image/png' }
    @{ Source = 'website/screenshots/streaming.png'; Name = 'screenshots/streaming.png'; Type = 'image/png' }
    @{ Source = 'website/screenshots/add-friend.png'; Name = 'screenshots/add-friend.png'; Type = 'image/png' }
    @{ Source = 'website/screenshots/requests.png'; Name = 'screenshots/requests.png'; Type = 'image/png' }
    @{ Source = 'website/screenshots/requests-incoming.png'; Name = 'screenshots/requests-incoming.png'; Type = 'image/png' }
    @{ Source = 'website/404.html'; Name = '404.html'; Type = 'text/html; charset=utf-8' }
    @{ Source = 'assets/logo.png'; Name = 'logo.png'; Type = 'image/png' }
    @{ Source = 'website/index.html'; Name = 'index.html'; Type = 'text/html; charset=utf-8' }
)
foreach ($upload in $uploads) {
    $upload.Path = Join-Path $root $upload.Source
    if (-not (Test-Path -LiteralPath $upload.Path -PathType Leaf)) {
        throw "Missing website asset: $($upload.Path)"
    }
}
Get-Command az -ErrorAction Stop | Out-Null

# GitHub releases are private. Fetch the public manifest before any mutation so
# a failed release lookup cannot replace the site's working download snapshot.
$releaseBase = 'https://orangealpha0d8d5893e69a3.blob.core.windows.net/releases/'
$release = Invoke-RestMethod -Uri "${releaseBase}orange-beta.json" -Method Get -TimeoutSec 20
$versionPattern = '(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)'
if ($null -eq $release -or $release -is [Array] -or
    ($release.schema -isnot [int] -and $release.schema -isnot [long]) -or $release.schema -ne 1 -or
    $release.channel -cne 'beta' -or $release.version -isnot [string] -or
    $release.version -cnotmatch "\A$versionPattern\z" -or
    $release.installer_url -isnot [string] -or
    $release.installer_url -cne "${releaseBase}orange-setup-$($release.version).exe") {
    throw 'Invalid public beta release manifest'
}

$indexUpload = $uploads[-1]
$html = [IO.File]::ReadAllText($indexUpload.Path)
$downloadPattern = '(?<=\shref=")' + [Regex]::Escape($releaseBase) + 'orange-setup-' + $versionPattern + '\.exe(?=")'
$fallbackPattern = '(?<=<span data-fallback-version>)' + $versionPattern + '(?=</span>)'
if ([Regex]::Matches($html, $downloadPattern).Count -ne 2 -or
    [Regex]::Matches($html, $fallbackPattern).Count -ne 1) {
    throw 'Website index must contain exactly two trusted installer hrefs and one fallback version span'
}
$html = [Regex]::Replace($html, $downloadPattern, $release.installer_url)
$html = [Regex]::Replace($html, $fallbackPattern, $release.version)

$stagedIndex = $null
try {
    $stagedIndex = [IO.Path]::GetTempFileName()
    [IO.File]::WriteAllText($stagedIndex, $html, (New-Object Text.UTF8Encoding($false)))
    $indexUpload.Path = $stagedIndex

    Invoke-WebsiteAzure @(
        'storage', 'blob', 'service-properties', 'update',
        '--account-name', $storageAccount, '--auth-mode', 'key',
        '--static-website', 'true', '--index-document', 'index.html', '--404-document', '404.html'
    ) | Out-Null

    # Azure assigns a regional web endpoint; it cannot be derived from the blob URL.
    $account = Invoke-WebsiteAzure @(
        'storage', 'account', 'show', '--name', $storageAccount, '--resource-group', $resourceGroup
    )
    $endpoint = [string]$account.primaryEndpoints.web
    $uri = $null
    if (-not [Uri]::TryCreate($endpoint, [UriKind]::Absolute, [ref]$uri) -or $uri.Scheme -ne 'https') {
        throw 'Azure did not return an HTTPS static website endpoint'
    }
    $origin = $uri.GetLeftPart([UriPartial]::Authority)

    $properties = Invoke-WebsiteAzure @(
        'storage', 'blob', 'service-properties', 'show', '--account-name', $storageAccount, '--auth-mode', 'key'
    )
    if ($null -eq $properties -or $null -eq $properties.PSObject.Properties['cors']) {
        throw 'Azure did not return blob service CORS properties'
    }
    $allowsRead = $false
    foreach ($rule in $properties.cors) {
        # CLI versions serialize these fields as lists or comma-separated strings.
        $methods = @($rule.allowedMethods) -join ','
        if (($methods -split ',' | ForEach-Object { $_.Trim() }) -notcontains 'GET') { continue }
        foreach ($allowedOrigin in ((@($rule.allowedOrigins) -join ',') -split ',')) {
            # Storage supports both a full wildcard and wildcard subdomain origins.
            $pattern = '^' + [Regex]::Escape($allowedOrigin.Trim()).Replace('\*', '.*') + '$'
            if ([Regex]::IsMatch($origin, $pattern)) { $allowsRead = $true; break }
        }
        if ($allowsRead) { break }
    }
    if (-not $allowsRead) {
        # cors add preserves unrelated rules. Unlike blob commands, it has no
        # --auth-mode parameter and obtains the account key through the CLI login.
        Invoke-WebsiteAzure @(
            'storage', 'cors', 'add', '--account-name', $storageAccount,
            '--services', 'b', '--methods', 'GET', 'HEAD', '--origins', $origin, '--max-age', '3600'
        ) | Out-Null
    }

    foreach ($upload in $uploads) {
        Invoke-WebsiteAzure @(
            'storage', 'blob', 'upload', '--account-name', $storageAccount, '--auth-mode', 'key',
            '--container-name', '$web', '--name', $upload.Name, '--file', $upload.Path,
            '--overwrite', 'true', '--content-type', $upload.Type, '--content-cache-control', 'no-cache', '--no-progress'
        ) | Out-Null
    }

    Write-Host "Website deployed: $endpoint" -ForegroundColor Green
}
finally {
    if ($null -ne $stagedIndex -and (Test-Path -LiteralPath $stagedIndex)) {
        Remove-Item -LiteralPath $stagedIndex -Force
    }
}
