import assert from 'node:assert/strict';
import { existsSync, readFileSync } from 'node:fs';
import test from 'node:test';
import { fileURLToPath } from 'node:url';

import {
  buildCard,
  collectUsage,
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

test('builds a compact orange Discord card from available data', () => {
  const card = buildCard({
    asOf: new Date('2026-09-07T12:00:00Z'),
    validSessionAccounts: 12,
    loginAccounts: 2,
    blobGets: 17,
    release: { version: '1.0.9' },
  });
  assert.equal(card.embeds[0].color, 0xff5a1f);
  assert.deepEqual(card.embeds[0].fields.map(({ name }) => name), [
    '👤 Users',
    '⬇️ Download activity',
    '🚀 Release',
  ]);
  assert.match(card.embeds[0].fields[0].value, /12.*valid accounts/);
  assert.match(card.embeds[0].fields[2].value, /v1\.0\.9/);
});

test('omits unavailable metrics instead of rendering unavailable card fields', () => {
  const card = buildCard({
    asOf: new Date('2026-09-07T12:00:00Z'),
    validSessionAccounts: 2,
    loginAccounts: 1,
    blobGets: null,
    release: null,
  });
  assert.deepEqual(card.embeds[0].fields.map(({ name }) => name), ['👤 Users']);
  assert.doesNotMatch(JSON.stringify(card), /unavailable|egress/i);
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
  const sessionFilter = azCalls.find(
    (args) => args.includes("PartitionKey eq 'session'") && args.includes('--marker'),
  );
  assert.ok(sessionFilter);
  const metricFilters = azCalls
    .filter((args) => args[0] === 'monitor')
    .map((args) => args[args.indexOf('--filter') + 1]);
  assert.deepEqual(metricFilters, ["ApiName eq 'GetBlob' and ResponseType eq 'Success'"]);
});

test('posts only the Discord card payload to a webhook', async () => {
  let request;
  const payload = buildCard({
    asOf: new Date('2026-09-07T12:00:00Z'),
    validSessionAccounts: 2,
    loginAccounts: 1,
  });
  await postWebhook(
    'https://discord.com/api/webhooks/123/token',
    payload,
    async (url, options) => {
      request = { url, options };
      return { ok: true, status: 204 };
    },
  );
  assert.equal(request.url.hostname, 'discord.com');
  assert.deepEqual(JSON.parse(request.options.body), payload);
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
