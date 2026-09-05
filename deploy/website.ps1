# Publishes the website through static hosting on the existing release storage
# account. Storage serves the files directly, with no additional compute tier.
#
#   az login
#   .\deploy\website.ps1
#
# Re-running overwrites the six public assets and adds CORS only if needed.
# The index is uploaded last so its dependencies exist before it becomes live.

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
    @{ Source = 'website/release.js'; Name = 'release.js'; Type = 'application/javascript; charset=utf-8' }
    @{ Source = 'website/scene.svg'; Name = 'scene.svg'; Type = 'image/svg+xml' }
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
        '--overwrite', 'true', '--content-type', $upload.Type, '--content-cache-control', 'no-cache'
    ) | Out-Null
}

Write-Host "Website deployed: $endpoint" -ForegroundColor Green
