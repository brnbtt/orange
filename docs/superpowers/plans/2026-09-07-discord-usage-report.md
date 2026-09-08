# Daily Discord Usage Report Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Send one truthful daily Orange usage summary to a Discord webhook using the existing Azure Table and Blob telemetry.

**Architecture:** A dependency-free Node script runs in GitHub Actions once per day. It authenticates to Azure through GitHub OIDC, counts persisted profile/session rows, reads Azure Storage `GetBlob` transaction and egress metrics, fetches the public release manifest, and posts a bounded text report to Discord. The webhook remains a GitHub secret and is never written to source, logs, or the report.

**Tech Stack:** Node.js 24, Azure CLI, Azure Monitor Storage metrics, Azure Table Storage query, GitHub Actions, Discord webhook JSON API.

**Spec:** This document is the implementation specification for the requested Orange daily Discord usage automation.

## Global Constraints

- Do not store or print the Discord webhook URL; read it only from `DISCORD_WEBHOOK_URL`.
- Use `azure/login@v3` with GitHub OIDC; do not create a long-lived Azure credential in the repository.
- Report distinct persisted profile rows as registered accounts and session rows as durable sessions.
- Report session rows created in the preceding 24 hours as login activity, not as unique new people.
- Report successful Blob `GetBlob` requests and egress as storage activity, never as unique downloaders.
- Say explicitly that Azure’s public static Blob endpoint cannot identify unique people downloading an installer.
- Keep the report under Discord’s 2,000-character content limit and fail without leaking secrets.

---

### Task 1: Build and test the report formatter

**Files:**
- Create: `automation/daily-usage-report.mjs`
- Create: `automation/daily-usage-report.test.mjs`

**Interfaces:**
- `normalizeRows(payload)` returns the entity array from Azure CLI table output.
- `sumMetric(payload)` returns the sum of Azure Monitor time-series `total` values.
- `formatBytes(bytes)` returns a compact human-readable byte count.
- `buildReport(data)` returns the bounded Discord message body.
- `validateWebhookUrl(value)` accepts only Discord webhook hosts and returns a `URL`.

- [ ] **Step 1: Write failing formatter tests**

```js
import test from 'node:test';
import assert from 'node:assert/strict';
import {
  buildReport,
  formatBytes,
  normalizeRows,
  sumMetric,
  validateWebhookUrl,
} from './daily-usage-report.mjs';

test('normalizes Azure CLI table results and markers', () => {
  assert.deepEqual(normalizeRows({ items: [{ RowKey: 'one' }] }), [{ RowKey: 'one' }]);
  assert.deepEqual(normalizeRows([{ RowKey: 'two' }]), [{ RowKey: 'two' }]);
});

test('sums Azure Monitor totals without treating missing data as an error', () => {
  assert.equal(sumMetric({ timeseries: [{ data: [{ total: 2 }, { total: 3 }] }] }), 5);
  assert.equal(sumMetric({ timeseries: [] }), 0);
});

test('formats bytes and keeps the report honest about unique downloaders', () => {
  assert.equal(formatBytes(1536), '1.5 KiB');
  const report = buildReport({
    asOf: new Date('2026-09-07T12:00:00Z'),
    registeredAccounts: 12,
    durableSessions: 4,
    loginSessions: 3,
    loginAccounts: 2,
    blobGets: 17,
    blobEgress: 2048,
    release: { version: '1.0.9' },
  });
  assert.match(report, /Registered Discord accounts: 12/);
  assert.match(report, /Unique downloaders: unavailable/);
  assert.ok(report.length <= 2000);
});

test('rejects non-Discord webhook URLs', () => {
  assert.throws(() => validateWebhookUrl('https://example.invalid/hook'), /Discord/);
});
```

- [ ] **Step 2: Run the focused test and verify it fails**

Run: `node --test automation/daily-usage-report.test.mjs`

Expected: FAIL because the report module and exported functions do not exist yet.

- [ ] **Step 3: Implement the pure formatter functions**

Implement the four exports above, use `null`/`unavailable` for missing optional metrics, and cap the final report to 2,000 characters without splitting a line. Keep the download caveat in the report text.

- [ ] **Step 4: Run the focused test and verify it passes**

Run: `node --test automation/daily-usage-report.test.mjs`

Expected: PASS.

---

### Task 2: Add Azure collection and Discord delivery

**Files:**
- Modify: `automation/daily-usage-report.mjs`
- Modify: `automation/daily-usage-report.test.mjs`

**Interfaces:**
- `queryTableRows({ account, table, filter, runAz })` paginates `az storage entity query` results.
- `collectUsage({ now, runAz, fetchImpl, config })` returns counts and optional release/metric values without exposing entity identities.
- `postWebhook(url, content, fetchImpl)` sends `{ username: "Orange usage", content }` and accepts Discord’s 204 response.
- `main()` reads `DISCORD_WEBHOOK_URL`, Azure defaults, and `DRY_RUN`/`--dry-run`.

- [ ] **Step 1: Add failing tests for pagination, session windows, metric filters, and webhook payloads**

Use injected `runAz` and `fetchImpl` fakes. Assert that profile and session filters are `PartitionKey eq 'profile'` and `PartitionKey eq 'session'`, that `ApiName eq 'GetBlob' and ResponseType eq 'Success'` is used for both metrics, that two table pages are combined, and that the webhook request contains no Azure rows or credentials.

- [ ] **Step 2: Run the focused test and verify the new cases fail**

Run: `node --test automation/daily-usage-report.test.mjs`

Expected: the new collection and delivery tests fail before their implementation exists.

- [ ] **Step 3: Implement the Azure collector**

Run Azure CLI through `spawnSync` with `--only-show-errors --output json`. Query only the required fields, keep account IDs in memory for distinct-counting, and never print rows. Count active durable sessions using the existing 30-day TTL and count the prior-24-hour session rows and distinct IDs. Sum `Transactions` and `Egress` time series with the `GetBlob`/successful-response filter; render unavailable metrics instead of inventing zero when Azure rejects a metric query.

- [ ] **Step 4: Implement the webhook sender and CLI entry point**

Validate `DISCORD_WEBHOOK_URL` against `discord.com` or `discordapp.com` webhook hosts, post with `fetch`, use a 15-second timeout, and never include the URL in an error. `--dry-run` prints only the report; the normal path posts it. Exit non-zero on missing configuration, Azure table failure, or a non-2xx webhook response.

- [ ] **Step 5: Run the focused tests and verify they pass**

Run: `node --test automation/daily-usage-report.test.mjs`

Expected: PASS with no network calls because all Azure and HTTP boundaries are injected.

---

### Task 3: Schedule the report and document one-time setup

**Files:**
- Create: `.github/workflows/daily-usage-report.yml`
- Modify: `.github/workflows/ci.yml`
- Modify: `README.md`
- Create: `docs/operations/daily-usage-report.md`

**Interfaces:**
- The workflow runs at `12:00 UTC` daily and supports manual `workflow_dispatch` with a dry-run switch.
- Required GitHub secrets are `DISCORD_WEBHOOK_URL`, `AZURE_CLIENT_ID`, `AZURE_TENANT_ID`, and `AZURE_SUBSCRIPTION_ID`.
- The OIDC service principal needs `Reader` and `Monitoring Reader` on the release storage account plus `Storage Table Data Reader` on the `sessions` table/account.

- [ ] **Step 1: Write the workflow contract test**

Add a Node test that parses the workflow text and asserts the daily cron, `id-token: write`, `azure/login@v3`, all four secret references, `DRY_RUN`, and the script invocation. The test must also assert that the webhook URL is not present in the workflow.

- [ ] **Step 2: Run the workflow contract test and verify it fails**

Run: `node --test automation/daily-usage-report.test.mjs`

Expected: FAIL because the workflow file does not exist.

- [ ] **Step 3: Add the scheduled workflow**

Use `actions/checkout@v4`, `actions/setup-node@v4` with Node 24, `azure/login@v3` with OIDC secrets, and a Node run step with `DISCORD_WEBHOOK_URL` and `DRY_RUN`. Set `AZURE_CORE_OUTPUT=none`, `contents: read`, `id-token: write`, a ten-minute timeout, and a concurrency group that prevents overlapping reports.

- [ ] **Step 4: Document Azure role setup, GitHub secrets, schedule, and metric limits**

Document OIDC federated-credential setup, the three least-privilege Azure read roles, `gh secret set` commands using placeholders only, manual dry-run invocation, the default Brazil-friendly UTC time, and the fact that Blob GETs/egress are activity estimates rather than unique people or installer-only downloads.

- [ ] **Step 5: Run all automation tests**

Run: `node --test automation/daily-usage-report.test.mjs`

Expected: PASS.

---

### Task 4: Run repository verification

**Files:**
- Verify all changed files; do not modify Rust or release versions for this automation.

- [ ] **Step 1: Run the website and automation JavaScript tests**

Run: `node --test website/release.test.mjs website/i18n.test.mjs automation/daily-usage-report.test.mjs`

Expected: PASS.

- [ ] **Step 2: Run formatting and the media-free Rust gate**

Run: `. .\dev.ps1; cargo fmt --all -- --check; cargo test --locked -p orange-signal -p orange-relay`

Expected: PASS; the automation is outside the Rust workspace and must not change its baseline.

- [ ] **Step 3: Review the diff for secrets and workflow correctness**

Run: `git diff --check` and search the changed files for `api/webhooks`, `DISCORD_WEBHOOK_URL`, and `AZURE_`.

Expected: only the environment variable name and placeholder setup references appear; the supplied webhook value never appears.
