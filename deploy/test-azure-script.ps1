$ErrorActionPreference = "Stop"

$scriptPath = Join-Path $PSScriptRoot "azure.ps1"
$source = Get-Content -LiteralPath $scriptPath -Raw
$checks = [ordered]@{
    "secure transfer" = '--https-only\s+true'
    "TLS 1.2" = '--min-tls-version\s+TLS1_2'
    "public blob access disabled" = '--allow-blob-public-access\s+false'
    "StorageV2" = '--kind\s+StorageV2'
    "Standard LRS" = '--sku\s+Standard_LRS'
    "private container" = 'container create[\s\S]*--public-access\s+off'
    "lifecycle policy" = 'management-policy create'
    "thirty day default" = '\[int\]\$DiagnosticsRetentionDays\s*=\s*30'
    "write-only SAS" = '--permissions\s+acw(?:\s|`)'
    "HTTPS-only SAS" = 'generate-sas[\s\S]*--https-only'
    "short secret name" = 'diag-container-url'
    "secret environment reference" = 'ORANGE_DIAGNOSTICS_CONTAINER_URL=secretref:diag-container-url'
    "public alpha account" = 'orangealpha0d8d5893e69a3'
    "public alpha container" = 'container create[\s\S]*--name\s+releases[\s\S]*--public-access\s+blob'
}

$failures = New-Object System.Collections.Generic.List[string]
foreach ($check in $checks.GetEnumerator()) {
    if ($source -notmatch $check.Value) {
        $failures.Add($check.Key)
    }
}
if ($source -match '(Write-(Host|Output)|echo)[^\r\n]*(\$sas|containerSas|diagnosticsUrl)') {
    $failures.Add("SAS value is printed")
}

$tokens = $null
$errors = $null
[System.Management.Automation.Language.Parser]::ParseFile($scriptPath, [ref]$tokens, [ref]$errors) | Out-Null
if ($errors.Count -ne 0) {
    $failures.Add("deployment script parser errors: $($errors.Message -join '; ')")
}

if ($failures.Count -ne 0) {
    throw "Azure deployment checks failed: $($failures -join ', ')"
}
Write-Host "RESULT: Azure deployment checks passed"
