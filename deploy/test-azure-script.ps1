$ErrorActionPreference = "Stop"

$scriptPath = Join-Path $PSScriptRoot "azure.ps1"
$source = Get-Content -LiteralPath $scriptPath -Raw
$checks = [ordered]@{
    "secure transfer" = '--https-only\s+true'
    "TLS 1.2" = '--min-tls-version\s+TLS1_2'
    "StorageV2" = '--kind\s+StorageV2'
    "Standard LRS" = '--sku\s+Standard_LRS'
    # This account name is compiled into every shipped client
    # (orange-tray/src/update.rs). Changing it strands their update path.
    "pinned release account" = 'orangealpha0d8d5893e69a3'
    "public releases container" = 'container create[\s\S]*--name\s+releases[\s\S]*--public-access\s+blob'
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

if ($failures.Count -ne 0) {
    throw "Azure deployment checks failed: $($failures -join ', ')"
}
Write-Host "RESULT: Azure deployment checks passed"
