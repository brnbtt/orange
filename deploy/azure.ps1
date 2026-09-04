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
    # Holds every published installer and the live orange-beta.json manifest.
    # The name is baked into shipped clients (orange-client/src/update.rs) and so
    # can never change without stranding their update path. The "alpha" in it is
    # historical; the alpha channel is gone.
    [string]$ReleaseStorageAccount = "orangealpha0d8d5893e69a3",
    # Sessions live here so a deploy stops signing everyone out.
    [string]$SessionTable = "sessions"
)

$ErrorActionPreference = "Stop"
$root = Split-Path $PSScriptRoot -Parent

function Step($msg) { Write-Host "==> $msg" -ForegroundColor Cyan }

# --- preflight ---------------------------------------------------------------
$account = az account show 2>$null | ConvertFrom-Json
if (-not $account) { throw "Not logged in. Run: az login" }
Write-Host "Subscription: $($account.name)" -ForegroundColor DarkGray

if ($ReleaseStorageAccount -cnotmatch '^[a-z0-9]{3,24}$') {
    throw "ReleaseStorageAccount must contain 3-24 lowercase letters or digits"
}

Step "Ensuring the containerapp extension is present"
az extension add --name containerapp --upgrade --only-show-errors 2>$null | Out-Null
az provider register --namespace Microsoft.App --only-show-errors 2>$null | Out-Null
az provider register --namespace Microsoft.OperationalInsights --only-show-errors 2>$null | Out-Null
az provider register --namespace Microsoft.Storage --only-show-errors 2>$null | Out-Null

Step "Resource group $ResourceGroup in $Location"
az group create --name $ResourceGroup --location $Location --only-show-errors | Out-Null

Step "Public release storage $ReleaseStorageAccount"
az storage account create `
    --name $ReleaseStorageAccount `
    --resource-group $ResourceGroup `
    --location $Location `
    --sku Standard_LRS `
    --kind StorageV2 `
    --https-only true `
    --min-tls-version TLS1_2 `
    --allow-blob-public-access true `
    --only-show-errors | Out-Null
if ($LASTEXITCODE -ne 0) { throw "release storage account provisioning failed" }

az storage container create `
    --name releases `
    --account-name $ReleaseStorageAccount `
    --auth-mode key `
    --public-access blob `
    --only-show-errors | Out-Null
if ($LASTEXITCODE -ne 0) { throw "release container provisioning failed" }

# Sessions outlive the relay process. Everything else it holds belongs to a
# connection and dies with it, but a session is the only thing a user cannot
# recreate without leaving the app, so keeping it in memory meant every deploy
# signed everyone out. Table Storage in the account that already exists: no new
# resource, no idle charge, and billed per operation on a few writes a day.
Step "Session table $SessionTable"
az storage table create `
    --name $SessionTable `
    --account-name $ReleaseStorageAccount `
    --auth-mode key `
    --only-show-errors | Out-Null
if ($LASTEXITCODE -ne 0) { throw "session table provisioning failed" }

$storageKey = az storage account keys list `
    --account-name $ReleaseStorageAccount `
    --resource-group $ResourceGroup `
    --query "[0].value" -o tsv
if ($LASTEXITCODE -ne 0 -or -not $storageKey) { throw "could not read the storage account key" }

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

Step "Session storage configuration"
# The key is a secret reference rather than a plain env var, so it does not
# appear in `az containerapp show` output or the portal's environment listing.
az containerapp secret set `
    --name $AppName `
    --resource-group $ResourceGroup `
    --secrets "table-key=$storageKey" `
    --only-show-errors | Out-Null
if ($LASTEXITCODE -ne 0) { throw "storing the table key failed" }

az containerapp update `
    --name $AppName `
    --resource-group $ResourceGroup `
    --set-env-vars `
        "ORANGE_TABLE_ACCOUNT=$ReleaseStorageAccount" `
        "ORANGE_TABLE_NAME=$SessionTable" `
        "ORANGE_TABLE_KEY=secretref:table-key" `
    --only-show-errors | Out-Null
if ($LASTEXITCODE -ne 0) { throw "session storage configuration failed" }

Step "Pinning to a single always-on replica"
# One replica keeps WebSocket state coherent: rooms live in memory, so two
# replicas behind the same ingress could put host and viewer on different
# instances and they would never find each other. Sessions are durable now,
# but rooms are not, so this constraint is unchanged.
az containerapp update `
    --name $AppName `
    --resource-group $ResourceGroup `
    --min-replicas 1 `
    --max-replicas 1 `
    --only-show-errors | Out-Null
if ($LASTEXITCODE -ne 0) { throw "replica pinning failed" }

# --- report ------------------------------------------------------------------
$fqdn = az containerapp show --name $AppName --resource-group $ResourceGroup `
    --query "properties.configuration.ingress.fqdn" -o tsv
if ($LASTEXITCODE -ne 0 -or -not $fqdn) { throw "could not read the relay hostname" }

Write-Host ""
Write-Host "Relay deployed." -ForegroundColor Green
Write-Host "  wss://$fqdn" -ForegroundColor Yellow
Write-Host ""
Write-Host "Host:  orange host --hwnd <id> --server wss://$fqdn"
Write-Host "Watch: orange watch --code <CODE> --server wss://$fqdn"
Write-Host ""
Write-Host "Deploying drops every live room: rooms are in memory. Sessions now survive." -ForegroundColor DarkGray
Write-Host ""
Write-Host "Do NOT tear down with 'az group delete --name $ResourceGroup'." -ForegroundColor Yellow
Write-Host "That would also destroy $ReleaseStorageAccount, which holds every published" -ForegroundColor DarkGray
Write-Host "installer and the live orange-beta.json. Delete the container app alone with:" -ForegroundColor DarkGray
Write-Host "  az containerapp delete --name $AppName --resource-group $ResourceGroup --yes" -ForegroundColor DarkGray
