import { spawnSync } from 'node:child_process';
import { pathToFileURL } from 'node:url';
import { resolve } from 'node:path';

const DAY_MS = 24 * 60 * 60 * 1000;
const SESSION_TTL_MS = 30 * DAY_MS;
const WEBHOOK_TIMEOUT_MS = 15_000;
const MAX_TABLE_PAGES = 128;
const EMBED_COLOR = 0xff5a1f;
const AZURE_COMMAND = process.platform === 'win32' ? (process.env.ComSpec || 'cmd.exe') : 'az';
const AZURE_PREFIX = process.platform === 'win32' ? ['/d', '/s', '/c', 'az.cmd'] : [];

export const DEFAULT_CONFIG = Object.freeze({
  account: 'orangealpha0d8d5893e69a3',
  resourceGroup: 'orange-rg',
  table: 'sessions',
  manifestUrl:
    'https://orangealpha0d8d5893e69a3.blob.core.windows.net/releases/orange-beta.json',
});

/**
 * Azure CLI returns table query results as { items, nextMarker }. Keeping this
 * tolerant of the REST-shaped { value } form makes the parser easy to test and
 * protects the report from a CLI output-shape change.
 */
export function normalizeRows(payload) {
  if (Array.isArray(payload)) return payload;
  if (Array.isArray(payload?.items)) return payload.items;
  if (Array.isArray(payload?.value)) return payload.value;
  return [];
}

/** Sum the total values returned by az monitor metrics list. */
export function sumMetric(payload) {
  const results = Array.isArray(payload)
    ? payload
    : Array.isArray(payload?.value)
      ? payload.value
      : payload
        ? [payload]
        : [];
  let total = 0;
  for (const result of results) {
    const series = result?.timeseries || result?.timeSeries || [];
    for (const entry of series) {
      for (const point of entry?.data || []) {
        const value = Number(point?.total ?? 0);
        if (Number.isFinite(value)) total += value;
      }
    }
  }
  return total;
}

function displayNumber(value) {
  return Number.isFinite(value) ? Math.round(value).toLocaleString('en-US') : 'unavailable';
}

export function buildCard(data) {
  const asOf = data.asOf instanceof Date ? data.asOf : new Date(data.asOf);
  const fields = [];
  const userLines = [];
  if (Number.isFinite(data.validSessionAccounts)) {
    userLines.push(`**${displayNumber(data.validSessionAccounts)}** valid accounts (30d)`);
  }
  if (Number.isFinite(data.loginAccounts)) {
    userLines.push(`**${displayNumber(data.loginAccounts)}** accounts logged in (24h)`);
  }
  if (userLines.length) {
    fields.push({ name: '👤 Users', value: userLines.join('\n'), inline: true });
  }
  if (Number.isFinite(data.blobGets)) {
    fields.push({
      name: '⬇️ Download activity',
      value: `**${displayNumber(data.blobGets)}** successful public Blob GETs\nUnique people are not identifiable`,
      inline: true,
    });
  }
  if (data.release?.version) {
    fields.push({ name: '🚀 Release', value: `**v${data.release.version}**`, inline: true });
  }
  return {
    username: 'Orange usage',
    allowed_mentions: { parse: [] },
    embeds: [{
      title: 'Orange daily usage',
      description: `Last 24 hours • <t:${Math.floor(asOf.getTime() / 1000)}:D>`,
      color: EMBED_COLOR,
      fields,
      timestamp: asOf.toISOString(),
      footer: {
        text: Number.isFinite(data.blobGets)
          ? 'Blob GETs include website assets; they are not unique downloads.'
          : 'Orange usage telemetry',
      },
    }],
  };
}

export function validateWebhookUrl(value) {
  let url;
  try {
    url = new URL(value);
  } catch {
    throw new Error('DISCORD_WEBHOOK_URL is not a valid Discord webhook URL');
  }
  const hostname = url.hostname.toLowerCase();
  if (url.protocol !== 'https:') {
    throw new Error('DISCORD_WEBHOOK_URL must use HTTPS');
  }
  if (!['discord.com', 'discordapp.com'].includes(hostname) ||
      !url.pathname.startsWith('/api/webhooks/')) {
    throw new Error('DISCORD_WEBHOOK_URL must be a Discord webhook URL');
  }
  return url;
}

function runAzJson(args) {
  const result = spawnSync(
    AZURE_COMMAND,
    [...AZURE_PREFIX, ...args, '--only-show-errors', '--output', 'json'],
    {
      encoding: 'utf8',
      maxBuffer: 16 * 1024 * 1024,
    },
  );
  if (result.error) throw new Error(`Azure CLI is unavailable: ${result.error.message}`);
  if (result.status !== 0) {
    throw new Error(`Azure CLI command failed: az ${args.slice(0, 3).join(' ')}`);
  }
  const output = result.stdout.trim();
  return output ? JSON.parse(output) : null;
}

function propertyValue(row, name) {
  const value = row?.[name] ?? row?.[name.toLowerCase()];
  if (value && typeof value === 'object' && 'value' in value) return value.value;
  return value;
}

function nextMarker(payload) {
  const marker = payload?.nextMarker ?? payload?.next_marker;
  if (!marker || typeof marker !== 'object') return null;
  const partition = marker.nextpartitionkey ?? marker.nextPartitionKey ?? marker.PartitionKey;
  const row = marker.nextrowkey ?? marker.nextRowKey ?? marker.RowKey;
  if (!partition && !row) return null;
  return { partition: String(partition || ''), row: String(row || '') };
}

export function queryTableRows({ account, table, filter, select, runAz = runAzJson }) {
  const rows = [];
  let marker = null;
  for (let page = 0; page < MAX_TABLE_PAGES; page += 1) {
    const args = [
      'storage', 'entity', 'query',
      '--account-name', account,
      '--table-name', table,
      '--auth-mode', 'login',
      '--filter', filter,
      '--select', ...select,
      '--num-results', '1000',
    ];
    if (marker) {
      args.push('--marker', `nextpartitionkey=${marker.partition}`, `nextrowkey=${marker.row}`);
    }
    const payload = runAz(args);
    rows.push(...normalizeRows(payload));
    const following = nextMarker(payload);
    if (!following || (following.partition === marker?.partition && following.row === marker?.row)) {
      return rows;
    }
    marker = following;
  }
  throw new Error('Azure Table query exceeded its pagination limit');
}

function createdAt(row) {
  const raw = propertyValue(row, 'CreatedAt');
  const seconds = Number(raw);
  if (!Number.isFinite(seconds) || seconds < 0) return null;
  const date = new Date(seconds * 1000);
  return Number.isNaN(date.getTime()) ? null : date;
}

function rowId(row) {
  const id = propertyValue(row, 'Id') ?? propertyValue(row, 'RowKey');
  return id === undefined || id === null ? '' : String(id);
}

function distinctIds(rows) {
  return new Set(rows.map(rowId).filter(Boolean));
}

function metricArgs(resourceId, metric, start, end) {
  return [
    'monitor', 'metrics', 'list',
    '--resource', resourceId,
    '--metrics', metric,
    '--aggregation', 'Total',
    '--interval', 'PT1H',
    '--start-time', start.toISOString(),
    '--end-time', end.toISOString(),
    '--filter', "ApiName eq 'GetBlob' and ResponseType eq 'Success'",
  ];
}

function storageResourceId(config) {
  const subscriptionId = config.subscriptionId || process.env.AZURE_SUBSCRIPTION_ID;
  if (!subscriptionId) throw new Error('AZURE_SUBSCRIPTION_ID is required');
  return `/subscriptions/${subscriptionId}/resourceGroups/${config.resourceGroup}` +
    `/providers/Microsoft.Storage/storageAccounts/${config.account}`;
}

async function optionalMetric({ resourceId, metric, start, end, runAz }) {
  try {
    return sumMetric(runAz(metricArgs(resourceId, metric, start, end)));
  } catch {
    return null;
  }
}

async function releaseVersion(url, fetchImpl) {
  try {
    const response = await fetchImpl(url, {
      signal: AbortSignal.timeout(10_000),
      headers: { accept: 'application/json' },
    });
    if (!response.ok) return null;
    const release = await response.json();
    if (release?.schema !== 1 || release?.channel !== 'beta' ||
        typeof release.version !== 'string' || !/^\d+\.\d+\.\d+$/.test(release.version)) {
      return null;
    }
    return { version: release.version };
  } catch {
    return null;
  }
}

export async function collectUsage({
  now = new Date(),
  runAz = runAzJson,
  fetchImpl = fetch,
  config = DEFAULT_CONFIG,
} = {}) {
  const asOf = now instanceof Date ? now : new Date(now);
  if (Number.isNaN(asOf.getTime())) throw new Error('report time is invalid');
  const periodStart = new Date(asOf.getTime() - DAY_MS);
  const profiles = queryTableRows({
    account: config.account,
    table: config.table,
    filter: "PartitionKey eq 'profile'",
    select: ['PartitionKey', 'RowKey'],
    runAz,
  });
  const sessions = queryTableRows({
    account: config.account,
    table: config.table,
    filter: "PartitionKey eq 'session'",
    select: ['PartitionKey', 'RowKey', 'Id', 'CreatedAt'],
    runAz,
  });
  const activeCutoff = new Date(asOf.getTime() - SESSION_TTL_MS);
  const recentSessions = sessions.filter((row) => {
    const date = createdAt(row);
    return date && date >= periodStart && date <= asOf;
  });
  const activeSessions = sessions.filter((row) => {
    const date = createdAt(row);
    return date && date >= activeCutoff && date <= asOf;
  });
  const resourceId = storageResourceId(config);
  const blobGets = await optionalMetric({
    resourceId,
    metric: 'Transactions',
    start: periodStart,
    end: asOf,
    runAz,
  });
  const release = await releaseVersion(config.manifestUrl, fetchImpl);
  return {
    asOf,
    validSessionAccounts: distinctIds(activeSessions).size,
    registeredProfiles: distinctIds(profiles).size || profiles.length,
    durableSessions: activeSessions.length,
    loginSessions: recentSessions.length,
    loginAccounts: distinctIds(recentSessions).size,
    blobGets,
    release,
  };
}

export async function postWebhook(value, payload, fetchImpl = fetch) {
  const url = validateWebhookUrl(value);
  if (!payload || !Array.isArray(payload.embeds) || payload.embeds.length === 0) {
    throw new Error('Discord webhook payload must contain an embed');
  }
  const body = JSON.stringify(payload);
  if (body.length > 6000) throw new Error('Discord webhook embed is too large');
  const response = await fetchImpl(url, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body,
    signal: AbortSignal.timeout(WEBHOOK_TIMEOUT_MS),
  });
  if (!response.ok) throw new Error(`Discord webhook failed with HTTP ${response.status}`);
}

function configFromEnvironment() {
  return {
    ...DEFAULT_CONFIG,
    account: process.env.ORANGE_STORAGE_ACCOUNT || DEFAULT_CONFIG.account,
    resourceGroup: process.env.ORANGE_RESOURCE_GROUP || DEFAULT_CONFIG.resourceGroup,
    table: process.env.ORANGE_SESSION_TABLE || DEFAULT_CONFIG.table,
    subscriptionId: process.env.AZURE_SUBSCRIPTION_ID,
    manifestUrl: process.env.ORANGE_RELEASE_MANIFEST_URL || DEFAULT_CONFIG.manifestUrl,
  };
}

export async function main() {
  const dryRun = process.argv.includes('--dry-run') || process.env.DRY_RUN === 'true';
  const webhook = process.env.DISCORD_WEBHOOK_URL;
  if (!dryRun && !webhook) throw new Error('DISCORD_WEBHOOK_URL is required');
  const card = buildCard(await collectUsage({ config: configFromEnvironment() }));
  if (dryRun) {
    console.log(JSON.stringify(card, null, 2));
    return;
  }
  await postWebhook(webhook, card);
  console.log('Daily Orange usage report sent to Discord');
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  main().catch((error) => {
    console.error(`[daily-usage-report] ${error instanceof Error ? error.message : String(error)}`);
    process.exitCode = 1;
  });
}
