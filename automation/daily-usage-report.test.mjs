import assert from 'node:assert/strict';
import { existsSync, readFileSync } from 'node:fs';
import test from 'node:test';
import { fileURLToPath } from 'node:url';

import {
  buildReport,
  collectUsage,
  formatBytes,
  normalizeRows,
  postWebhook,
  sumMetric,
  validateWebhookUrl,
} from './daily-usage-report.mjs';

test('normalizes Azure CLI table results', () => {
  assert.deepEqual(normalizeRows({ items: [{ RowKey: 'one' }] }), [{ RowKey: 'one' }]);
  assert.deepEqual(normalizeRows([{ RowKey: 'two' }]), [{ RowKey: 'two' }]);
  assert.deepEqual(normalizeRows({ value: [{ RowKey: 'three' }] }), [{ RowKey: 'three' }]);
});

test('sums Azure Monitor totals without treating missing data as an error', () => {
  assert.equal(
    sumMetric({
      timeseries: [{ data: [{ total: 2 }, { total: 3 }] }],
    }),
    5,
  );
  assert.equal(sumMetric({ timeseries: [] }), 0);
  assert.equal(sumMetric(null), 0);
});

test('formats bytes and keeps the report honest about unique downloaders', () => {
  assert.equal(formatBytes(1536), '1.5 KiB');
  const report = buildReport({
    asOf: new Date('2026-09-07T12:00:00Z'),
    validSessionAccounts: 12,
    registeredProfiles: 10,
    durableSessions: 4,
    loginSessions: 3,
    loginAccounts: 2,
    blobGets: 17,
    blobEgress: 2048,
    release: { version: '1.0.9' },
  });
  assert.match(report, /Accounts with a valid session \(30d\): 12/);
  assert.match(report, /Registered Discord profiles: 10/);
  assert.match(report, /Unique downloaders: unavailable/);
  assert.match(report, /Current beta release: 1\.0\.9/);
  assert.ok(report.length <= 2000);
});

test('rejects non-Discord webhook URLs', () => {
  assert.throws(() => validateWebhookUrl('https://example.invalid/hook'), /Discord/);
  assert.throws(() => validateWebhookUrl('http://discord.com/api/webhooks/1/token'), /HTTPS/);
});

test('collects paginated account/session rows and filtered storage metrics', async () => {
  const now = new Date('2026-09-07T12:00:00Z');
  const seconds = (value) => Math.floor(new Date(value).getTime() / 1000);
  const azCalls = [];
  const pages = new Map();
  const runAz = (args) => {
    azCalls.push(args);
    if (args[0] === 'storage' && args[1] === 'entity') {
      const filter = args[args.indexOf('--filter') + 1];
      const key = filter.includes('profile') ? 'profile' : 'session';
      const page = pages.get(key) || 0;
      pages.set(key, page + 1);
      if (key === 'profile') {
        return page === 0
          ? { items: [{ RowKey: '1' }], nextMarker: { nextpartitionkey: 'profile', nextrowkey: 'next' } }
          : { items: [{ RowKey: '2' }] };
      }
      const values = [
        { RowKey: 'a', Id: '10', CreatedAt: String(seconds('2026-09-07T11:00:00Z')) },
        { RowKey: 'b', Id: '10', CreatedAt: String(seconds('2026-09-06T13:00:00Z')) },
      ];
      return page === 0
        ? { items: values.slice(0, 1), nextMarker: { nextpartitionkey: 'session', nextrowkey: 'next' } }
        : { items: values.slice(1) };
    }
    if (args[0] === 'monitor' && args[args.indexOf('--metrics') + 1] === 'Transactions') {
      return [{ timeseries: [{ data: [{ total: 7 }] }] }];
    }
    if (args[0] === 'monitor' && args[args.indexOf('--metrics') + 1] === 'Egress') {
      return [{ timeseries: [{ data: [{ total: 1024 }] }] }];
    }
    throw new Error(`unexpected az call: ${args.join(' ')}`);
  };
  const fetchImpl = async () => ({
    ok: true,
    json: async () => ({ schema: 1, channel: 'beta', version: '1.0.9' }),
  });

  const data = await collectUsage({
    now,
    runAz,
    fetchImpl,
    config: {
      account: 'orange',
      resourceGroup: 'orange-rg',
      table: 'sessions',
      subscriptionId: 'test-subscription',
      manifestUrl: 'https://example.invalid/manifest.json',
    },
  });

  assert.equal(data.validSessionAccounts, 1);
  assert.equal(data.registeredProfiles, 2);
  assert.equal(data.durableSessions, 2);
  assert.equal(data.loginSessions, 2);
  assert.equal(data.loginAccounts, 1);
  assert.equal(data.blobGets, 7);
  assert.equal(data.blobEgress, 1024);
  const sessionFilter = azCalls.find(
    (args) => args.includes("PartitionKey eq 'session'") && args.includes('--marker'),
  );
  assert.ok(sessionFilter);
  const metricFilters = azCalls
    .filter((args) => args[0] === 'monitor')
    .map((args) => args[args.indexOf('--filter') + 1]);
  assert.deepEqual(metricFilters, [
    "ApiName eq 'GetBlob' and ResponseType eq 'Success'",
    "ApiName eq 'GetBlob' and ResponseType eq 'Success'",
  ]);
});

test('posts only the bounded report body to a Discord webhook', async () => {
  let request;
  await postWebhook(
    'https://discord.com/api/webhooks/123/token',
    'Orange report',
    async (url, options) => {
      request = { url, options };
      return { ok: true, status: 204 };
    },
  );
  assert.equal(request.url.hostname, 'discord.com');
  assert.deepEqual(JSON.parse(request.options.body), {
    username: 'Orange usage',
    content: 'Orange report',
  });
});

test('workflow schedules the report without embedding credentials', () => {
  const workflowPath = fileURLToPath(
    new URL('../.github/workflows/daily-usage-report.yml', import.meta.url),
  );
  assert.ok(existsSync(workflowPath));
  const workflow = readFileSync(workflowPath, 'utf8');
  assert.match(workflow, /cron:\s*['"]0 12 \* \* \*['"]/);
  assert.match(workflow, /id-token:\s*write/);
  assert.match(workflow, /azure\/login@v3/);
  assert.match(workflow, /secrets\.DISCORD_WEBHOOK_URL/);
  assert.match(workflow, /secrets\.AZURE_CLIENT_ID/);
  assert.match(workflow, /secrets\.AZURE_TENANT_ID/);
  assert.match(workflow, /secrets\.AZURE_SUBSCRIPTION_ID/);
  assert.match(workflow, /DRY_RUN/);
  assert.match(workflow, /node automation\/daily-usage-report\.mjs/);
  assert.doesNotMatch(workflow, /https:\/\/discord(?:app)?\.com\/api\/webhooks\//);
});
