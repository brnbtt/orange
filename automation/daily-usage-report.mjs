import { spawnSync } from 'node:child_process';
import { pathToFileURL } from 'node:url';
import { resolve } from 'node:path';

const DAY_MS = 24 * 60 * 60 * 1000;
const SESSION_TTL_MS = 30 * DAY_MS;
const REPORT_LIMIT = 2000;
const WEBHOOK_TIMEOUT_MS = 15_000;
const MAX_TABLE_PAGES = 128;
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

export function formatBytes(bytes) {
  if (!Number.isFinite(bytes)) return 'unavailable';
  if (bytes < 1024) return `${Math.round(bytes)} B`;
  const units = ['KiB', 'MiB', 'GiB', 'TiB'];
  let value = bytes;
  let unit = units[0];
  for (let index = 0; value >= 1024 && index < units.length; index += 1) {
    value /= 1024;
    unit = units[index];
  }
  const rounded = value >= 100 ? value.toFixed(0) : value.toFixed(1);
  return `${rounded.replace(/\.0$/, '')} ${unit}`;
}

function displayNumber(value) {
  return Number.isFinite(value) ? Math.round(value).toLocaleString('en-US') : 'unavailable';
}

function displayValue(value) {
  return value === null || value === undefined ? 'unavailable' : displayNumber(value);
}

function displayRelease(release) {
  return release?.version || 'unavailable';
}

/**
 * Keep this report deliberately plain text: it renders consistently in Discord
 * and leaves enough room for a useful warning when an optional Azure metric is
 * unavailable.
 */
export function buildReport(data) {
  const asOf = data.asOf instanceof Date ? data.asOf : new Date(data.asOf);
  const periodStart = new Date(asOf.getTime() - DAY_MS);
  const lines = [
    `🟠 Orange daily usage — ${asOf.toISOString().slice(0, 10)}`,
    `Period: ${periodStart.toISOString()} → ${asOf.toISOString()}`,
    '',
    '**Users**',
    `• Accounts with a valid session (30d): ${displayValue(data.validSessionAccounts ?? data.registeredAccounts)}`,
    `• Registered Discord profiles: ${displayValue(data.registeredProfiles ?? data.registeredAccounts)}`,
    `• Durable sessions: ${displayValue(data.durableSessions)}`,
    `• Accounts with a login in the last 24h: ${displayValue(data.loginAccounts)}`,
    `• Login sessions in the last 24h: ${displayValue(data.loginSessions)}`,
    '',
    '**Download activity**',
    `• Successful Blob GETs: ${displayValue(data.blobGets)}`,
    `• Blob egress: ${formatBytes(data.blobEgress)}`,
    '• Unique downloaders: unavailable (public Azure Blob downloads expose no person/device identity)',
    '',
    '• Anonymous launches and stream minutes: not collected',
    `• Current beta release: ${displayRelease(data.release)}`,
    '• Source: Azure Table Storage, Azure Monitor, and the public beta manifest',
  ];
  if (data.warnings?.length) {
    lines.push('', '**Notes**', ...data.warnings.map((warning) => `• ${warning}`));
  }
  let report = '';
  for (const line of lines) {
    const candidate = report ? `${report}\n${line}` : line;
    if (candidate.length > REPORT_LIMIT) break;
    report = candidate;
  }
  return report;
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

async function optionalMetric({ resourceId, metric, start, end, runAz, warnings }) {
  try {
    return sumMetric(runAz(metricArgs(resourceId, metric, start, end)));
  } catch {
    warnings.push(`${metric} storage metric unavailable`);
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
  const warnings = [];
  const resourceId = storageResourceId(config);
  const [blobGets, blobEgress] = await Promise.all([
    optionalMetric({ resourceId, metric: 'Transactions', start: periodStart, end: asOf, runAz, warnings }),
    optionalMetric({ resourceId, metric: 'Egress', start: periodStart, end: asOf, runAz, warnings }),
  ]);
  const release = await releaseVersion(config.manifestUrl, fetchImpl);
  if (!release) warnings.push('public beta manifest unavailable');
  return {
    asOf,
    validSessionAccounts: distinctIds(activeSessions).size,
    registeredProfiles: distinctIds(profiles).size || profiles.length,
    durableSessions: activeSessions.length,
    loginSessions: recentSessions.length,
    loginAccounts: distinctIds(recentSessions).size,
    blobGets,
    blobEgress,
    release,
    warnings,
  };
}

export async function postWebhook(value, content, fetchImpl = fetch) {
  const url = validateWebhookUrl(value);
  if (typeof content !== 'string' || content.length > REPORT_LIMIT) {
    throw new Error('Discord webhook content exceeds the 2,000-character limit');
  }
  const response = await fetchImpl(url, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ username: 'Orange usage', content }),
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
  const report = buildReport(await collectUsage({ config: configFromEnvironment() }));
  if (dryRun) {
    console.log(report);
    return;
  }
  await postWebhook(webhook, report);
  console.log('Daily Orange usage report sent to Discord');
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  main().catch((error) => {
    console.error(`[daily-usage-report] ${error instanceof Error ? error.message : String(error)}`);
    process.exitCode = 1;
  });
}
