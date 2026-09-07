$ErrorActionPreference = "Stop"

$scriptPath = Join-Path $PSScriptRoot "azure.ps1"
$source = Get-Content -LiteralPath $scriptPath -Raw
$checks = [ordered]@{
    "secure transfer" = '--https-only\s+true'
    "TLS 1.2" = '--min-tls-version\s+TLS1_2'
    "StorageV2" = '--kind\s+StorageV2'
    "Standard LRS" = '--sku\s+Standard_LRS'
    # This account name is compiled into every shipped client
    # (orange-client/src/update.rs). Changing it strands their update path.
    "pinned release account" = 'orangealpha0d8d5893e69a3'
    "public releases container" = 'container create[\s\S]*--name\s+releases[\s\S]*--public-access\s+blob'
    "private diagnostics container" = 'container create[\s\S]*--name\s+\$DiagnosticsContainer[\s\S]*--public-access\s+off'
    "diagnostics container permission pinned private" = 'container set-permission[\s\S]*--name\s+\$DiagnosticsContainer[\s\S]*--public-access\s+off'
    "diagnostics env var wired" = 'ORANGE_DIAGNOSTICS_CONTAINER=\$DiagnosticsContainer'
    "diagnostics no consecutive hyphens" = '\$DiagnosticsContainer\s+-match\s+''--'''
    "diagnostics reserved container guard" = '\$DiagnosticsContainer\s+-in\s+@\(''releases'', ''\$web''\)'
    "single replica pinned" = '--min-replicas\s+1[\s\S]*--max-replicas\s+1'
}

$failures = New-Object System.Collections.Generic.List[string]
foreach ($check in $checks.GetEnumerator()) {
    if ($source -notmatch $check.Value) {
        $failures.Add($check.Key)
    }
}

# The release storage account lives in the same resource group as the relay, so
# `az group delete` would destroy every published installer and the live
# manifest. The script must not suggest it.
if ($source -match 'Tear down with: az group delete') {
    $failures.Add("teardown hint deletes the release storage account")
}

$tokens = $null
$errors = $null
[System.Management.Automation.Language.Parser]::ParseFile($scriptPath, [ref]$tokens, [ref]$errors) | Out-Null
if ($errors.Count -ne 0) {
    $failures.Add("deployment script parser errors: $($errors.Message -join '; ')")
}

# Diagnostics uploads must never repurpose public content containers. This
# check executes only preflight validation with a mocked `az account show`.
$negativeScript = @"
function az {
    param([Parameter(ValueFromRemainingArguments = `$true)][string[]]`$Args)
    if (`$Args.Length -ge 2 -and `$Args[0] -eq 'account' -and `$Args[1] -eq 'show') {
        @{ name = 'mock' } | ConvertTo-Json -Compress
        return
    }
    throw "az should not run past diagnostics container preflight"
}
& '$scriptPath' -DiagnosticsContainer releases
"@
$previousErrorActionPreference = $ErrorActionPreference
$ErrorActionPreference = "Continue"
$negative = powershell -NoProfile -ExecutionPolicy Bypass -Command $negativeScript 2>&1 | Out-String
$negativeExitCode = $LASTEXITCODE
$ErrorActionPreference = $previousErrorActionPreference
if ($negativeExitCode -eq 0) {
    $failures.Add("reserved diagnostics container was accepted")
}
if ($negative -notmatch "DiagnosticsContainer cannot be a reserved or public content container") {
    $failures.Add("reserved diagnostics container rejection message missing")
}

if ($failures.Count -ne 0) {
    throw "Azure deployment checks failed: $($failures -join ', ')"
}
Write-Host "RESULT: Azure deployment checks passed"
