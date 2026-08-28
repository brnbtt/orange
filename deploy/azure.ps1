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
    [string]$Environment   = "orange-env"
)

$ErrorActionPreference = "Stop"
$root = Split-Path $PSScriptRoot -Parent

function Step($msg) { Write-Host "==> $msg" -ForegroundColor Cyan }

# --- preflight ---------------------------------------------------------------
$account = az account show 2>$null | ConvertFrom-Json
if (-not $account) { throw "Not logged in. Run: az login" }
Write-Host "Subscription: $($account.name)" -ForegroundColor DarkGray

Step "Ensuring the containerapp extension is present"
az extension add --name containerapp --upgrade --only-show-errors 2>$null | Out-Null
az provider register --namespace Microsoft.App --only-show-errors 2>$null | Out-Null
az provider register --namespace Microsoft.OperationalInsights --only-show-errors 2>$null | Out-Null

Step "Resource group $ResourceGroup in $Location"
az group create --name $ResourceGroup --location $Location --only-show-errors | Out-Null

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

Step "Pinning to a single always-on replica"
# One replica keeps WebSocket state coherent: rooms live in memory, so two
# replicas behind the same ingress could put host and viewer on different
# instances and they would never find each other.
az containerapp update `
    --name $AppName `
    --resource-group $ResourceGroup `
    --min-replicas 1 `
    --max-replicas 1 `
    --only-show-errors | Out-Null

# --- report ------------------------------------------------------------------
$fqdn = az containerapp show --name $AppName --resource-group $ResourceGroup `
    --query "properties.configuration.ingress.fqdn" -o tsv

Write-Host ""
Write-Host "Relay deployed." -ForegroundColor Green
Write-Host "  wss://$fqdn" -ForegroundColor Yellow
Write-Host ""
Write-Host "Host:  orange host --hwnd <id> --server wss://$fqdn"
Write-Host "Watch: orange watch --code <CODE> --server wss://$fqdn"
Write-Host ""
Write-Host "Tear down with: az group delete --name $ResourceGroup --yes" -ForegroundColor DarkGray
