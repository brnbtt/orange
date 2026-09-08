# Daily Discord usage report

The repository contains a scheduled GitHub Actions workflow at
`.github/workflows/daily-usage-report.yml`. It runs at 12:00 UTC (normally
09:00 in Brazil) and sends a compact orange embed card to the configured
Discord webhook.
The workflow can also be started manually with `dry_run=true`; that collects
and prints the message without sending it.

## What the report measures

The report deliberately uses facts the current Orange deployment already
stores:

- **Accounts with a valid session (30d)**: distinct account IDs in session rows
  that have not passed the relay's existing 30-day session lifetime. This is a
  count of valid authenticated accounts, not a count of people currently using
  the app.
- **Registered Discord profiles**: distinct rows in the `profile` partition of
  the durable `sessions` table. This is the set of accounts that have reached a
  profile-backed Orange feature, not anonymous installations.
- **Login activity**: session rows and distinct account IDs created during the
  preceding 24 hours. This is login activity, not a claim that each row is a
  new person.
- **Successful Blob GETs**: Azure Monitor Storage transaction metrics for
  successful `GetBlob` operations during the preceding 24 hours. They include
  the public website, manifest, installer, and other public blobs in the
  account.
- **Current beta**: the public `orange-beta.json` release manifest.

Azure static Blob hosting does not expose a person or device identity to this
job. Therefore the report says **unique downloaders: unavailable** instead of
pretending that Blob GETs are the number of people who downloaded the
installer. The current metrics also cannot distinguish installer GETs from
website asset GETs. Add a first-party, privacy-reviewed download counter if
exact unique installer downloads are required.

Anonymous launches and stream minutes are not collected by the current Orange
relay, so the card omits them rather than inferring usage from storage or
authentication traffic.

## One-time Azure setup

The workflow uses GitHub OIDC with `azure/login@v3`; it does not use a stored
Azure client secret. Create a service principal, configure a federated
credential for `brnbtt/orange` (the `main` branch or the repository's chosen
production environment), and save its client ID, tenant ID, and subscription
ID as GitHub Actions secrets.

Grant the service principal only the read roles needed by the report. Azure
Monitor requires resource read access in addition to its monitoring role:

```powershell
$storageId = az storage account show `
  --resource-group orange-rg `
  --name orangealpha0d8d5893e69a3 `
  --query id -o tsv

az role assignment create `
  --assignee <AZURE_CLIENT_ID> `
  --role Reader `
  --scope $storageId

az role assignment create `
  --assignee <AZURE_CLIENT_ID> `
  --role "Monitoring Reader" `
  --scope $storageId

az role assignment create `
  --assignee <AZURE_CLIENT_ID> `
  --role "Storage Table Data Reader" `
  --scope $storageId
```

The federated credential must match the workflow subject exactly. For a
branch-based trust that is typically:

```text
repo:brnbtt/orange:ref:refs/heads/main
```

Use the subject shown by the Azure Login action if the repository uses an
environment or a different branch trust. OIDC setup is intentionally left as
an Azure identity-administration step rather than creating credentials from a
workflow run.

## GitHub secrets

Set these four repository secrets. The webhook is entered interactively so it
never lands in the shell history or source tree:

```powershell
gh secret set DISCORD_WEBHOOK_URL --repo brnbtt/orange
gh secret set AZURE_CLIENT_ID --repo brnbtt/orange --body '<client-id>'
gh secret set AZURE_TENANT_ID --repo brnbtt/orange --body '<tenant-id>'
gh secret set AZURE_SUBSCRIPTION_ID --repo brnbtt/orange --body '<subscription-id>'
```

Because Discord webhook URLs are bearer credentials, rotate the webhook in
Discord if it has been shared somewhere public or in an issue/chat transcript.

## Manual verification

After the workflow is on the default branch and the secrets/roles exist, run a
dry collection:

```powershell
gh workflow run daily-usage-report.yml --repo brnbtt/orange -f dry_run=true
gh run watch --repo brnbtt/orange
```

The workflow's Azure CLI commands set `AZURE_CORE_OUTPUT=none`; only the
specific JSON responses consumed by the Node script are captured, and table
rows are never printed. A normal run fails rather than sending a misleading
report if the table query or Azure login fails. Optional metrics that are
unavailable are omitted from the card.
