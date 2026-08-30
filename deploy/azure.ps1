# Deploys the orange signalling relay to Azure Container Apps.
#
# The Dockerfile lives at the repo root because `az containerapp up --source` 
# requires it there; it will not accept a path to one elsewhere.
#
# Container Apps is a good fit here: it terminates TLS and hands out an HTTPS
# hostname for free (so clients get wss:// with no certificate work), it
# supports WebSockets, and the relay is small enough to sit in the cheapest
# consumption tier.
#
#   az login
#   .\deploy\azure.ps1
#
# Note min-replicas is 1, not 0. Scale-to-zero would be cheaper, but it kills
# idle WebSocket connections and adds a cold start to the first viewer join.

param(
    [string]$ResourceGroup = "orange-rg",
    [string]$Location      = "brazilsouth",
    [string]$AppName       = "orange-relay",
    [string]$Environment   = "orange-env",
    [string]$StorageAccount,
    [string]$DiagnosticsContainer = "diagnostics",
    [int]$DiagnosticsRetentionDays = 30,
    [string]$AlphaStorageAccount = "orangealpha0d8d5893e69a3"
)

$ErrorActionPreference = "Stop"
$root = Split-Path $PSScriptRoot -Parent

function Step($msg) { Write-Host "==> $msg" -ForegroundColor Cyan }

# --- preflight ---------------------------------------------------------------
$account = az account show 2>$null | ConvertFrom-Json
if (-not $account) { throw "Not logged in. Run: az login" }
Write-Host "Subscription: $($account.name)" -ForegroundColor DarkGray

if ($DiagnosticsRetentionDays -lt 1) {
    throw "DiagnosticsRetentionDays must be at least 1"
}
if ($DiagnosticsContainer -cnotmatch '^[a-z0-9](?:[a-z0-9-]{1,61}[a-z0-9])?$') {
    throw "DiagnosticsContainer must be a valid lowercase Azure container name"
}
if (-not $StorageAccount) {
    $sha256 = [Security.Cryptography.SHA256]::Create()
    try {
        $seed = [Text.Encoding]::UTF8.GetBytes("$($account.id)|$ResourceGroup|$AppName")
        $suffix = [BitConverter]::ToString($sha256.ComputeHash($seed)).Replace("-", "").ToLowerInvariant().Substring(0, 18)
        $StorageAccount = "orange$suffix"
    } finally {
        $sha256.Dispose()
    }
}
if ($StorageAccount -cnotmatch '^[a-z0-9]{3,24}$') {
    throw "StorageAccount must contain 3-24 lowercase letters or digits"
}
if ($AlphaStorageAccount -cnotmatch '^[a-z0-9]{3,24}$') {
    throw "AlphaStorageAccount must contain 3-24 lowercase letters or digits"
}

Step "Ensuring the containerapp extension is present"
az extension add --name containerapp --upgrade --only-show-errors 2>$null | Out-Null
az provider register --namespace Microsoft.App --only-show-errors 2>$null | Out-Null
az provider register --namespace Microsoft.OperationalInsights --only-show-errors 2>$null | Out-Null
az provider register --namespace Microsoft.Storage --only-show-errors 2>$null | Out-Null

Step "Resource group $ResourceGroup in $Location"
az group create --name $ResourceGroup --location $Location --only-show-errors | Out-Null

Step "Private diagnostics storage $StorageAccount"
az storage account create `
    --name $StorageAccount `
    --resource-group $ResourceGroup `
    --location $Location `
    --sku Standard_LRS `
    --kind StorageV2 `
    --https-only true `
    --min-tls-version TLS1_2 `
    --allow-blob-public-access false `
    --only-show-errors | Out-Null
if ($LASTEXITCODE -ne 0) { throw "storage account provisioning failed" }

az storage container create `
    --name $DiagnosticsContainer `
    --account-name $StorageAccount `
    --auth-mode login `
    --public-access off `
    --only-show-errors | Out-Null
if ($LASTEXITCODE -ne 0) { throw "diagnostics container provisioning failed" }

Step "Public alpha build storage $AlphaStorageAccount"
az storage account create `
    --name $AlphaStorageAccount `
    --resource-group $ResourceGroup `
    --location $Location `
    --sku Standard_LRS `
    --kind StorageV2 `
    --https-only true `
    --min-tls-version TLS1_2 `
    --allow-blob-public-access true `
    --only-show-errors | Out-Null
if ($LASTEXITCODE -ne 0) { throw "alpha storage account provisioning failed" }

az storage container create `
    --name releases `
    --account-name $AlphaStorageAccount `
    --auth-mode key `
    --public-access blob `
    --only-show-errors | Out-Null
if ($LASTEXITCODE -ne 0) { throw "alpha release container provisioning failed" }

$policyPath = Join-Path $env:TEMP ("orange-storage-policy-" + [guid]::NewGuid().ToString("N") + ".json")
try {
    @{
        rules = @(@{
            enabled = $true
            name = "delete-old-diagnostics"
            type = "Lifecycle"
            definition = @{
                filters = @{
                    blobTypes = @("blockBlob")
                    prefixMatch = @("$DiagnosticsContainer/")
                }
                actions = @{
                    baseBlob = @{
                        delete = @{ daysAfterModificationGreaterThan = $DiagnosticsRetentionDays }
                    }
                }
            }
        })
    } | ConvertTo-Json -Depth 10 | Set-Content -LiteralPath $policyPath -Encoding UTF8
    az storage account management-policy create `
        --account-name $StorageAccount `
        --resource-group $ResourceGroup `
        --policy "@$policyPath" `
        --only-show-errors | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "diagnostics retention policy provisioning failed" }
} finally {
    Remove-Item -LiteralPath $policyPath -Force -ErrorAction SilentlyContinue
}

$sasExpiry = [DateTime]::UtcNow.AddYears(1).ToString("yyyy-MM-ddTHH:mmZ")
$containerSas = az storage container generate-sas `
    --name $DiagnosticsContainer `
    --account-name $StorageAccount `
    --permissions acw `
    --expiry $sasExpiry `
    --https-only `
    --auth-mode key `
    --output tsv `
    --only-show-errors
if ($LASTEXITCODE -ne 0 -or -not $containerSas) { throw "diagnostics upload authorization failed" }
$blobEndpoint = az storage account show `
    --name $StorageAccount `
    --resource-group $ResourceGroup `
    --query "primaryEndpoints.blob" `
    --output tsv `
    --only-show-errors
if ($LASTEXITCODE -ne 0 -or -not $blobEndpoint) { throw "diagnostics Blob endpoint lookup failed" }
$diagnosticsUrl = $blobEndpoint.TrimEnd('/') + "/$DiagnosticsContainer`?" + $containerSas.TrimStart('?')

# --- deploy ------------------------------------------------------------------
# `--source` builds in the cloud, so no local Docker daemon is needed.
Step "Building and deploying (this takes a few minutes the first time)"
Push-Location $root
try {
    # Note: `containerapp up` rejects --only-show-errors, unlike most az commands.
    az containerapp up `
        --name $AppName `
        --resource-group $ResourceGroup `
        --location $Location `
        --environment $Environment `
        --source . `
        --ingress external `
        --target-port 9000
    if ($LASTEXITCODE -ne 0) { throw "az containerapp up failed" }
}
finally { Pop-Location }

Step "Configuring private diagnostics upload"
az containerapp secret set `
    --name $AppName `
    --resource-group $ResourceGroup `
    --secrets "diag-container-url=$diagnosticsUrl" `
    --only-show-errors | Out-Null
if ($LASTEXITCODE -ne 0) { throw "diagnostics secret configuration failed" }

Step "Pinning to a single always-on replica"
# One replica keeps WebSocket state coherent: rooms live in memory, so two
# replicas behind the same ingress could put host and viewer on different
# instances and they would never find each other.
az containerapp update `
    --name $AppName `
    --resource-group $ResourceGroup `
    --min-replicas 1 `
    --max-replicas 1 `
    --set-env-vars "ORANGE_DIAGNOSTICS_CONTAINER_URL=secretref:diag-container-url" `
    --only-show-errors | Out-Null

# --- report ------------------------------------------------------------------
$fqdn = az containerapp show --name $AppName --resource-group $ResourceGroup `
    --query "properties.configuration.ingress.fqdn" -o tsv

Write-Host ""
Write-Host "Relay deployed." -ForegroundColor Green
Write-Host "  wss://$fqdn" -ForegroundColor Yellow
Write-Host "  diagnostics: private Blob storage, $DiagnosticsRetentionDays-day retention" -ForegroundColor DarkGray
Write-Host ""
Write-Host "Host:  orange host --hwnd <id> --server wss://$fqdn"
Write-Host "Watch: orange watch --code <CODE> --server wss://$fqdn"
Write-Host ""
Write-Host "Tear down with: az group delete --name $ResourceGroup --yes" -ForegroundColor DarkGray
