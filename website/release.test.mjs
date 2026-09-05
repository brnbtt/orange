import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { runInNewContext } from 'node:vm';
import test from 'node:test';

const base = 'https://orangealpha0d8d5893e69a3.blob.core.windows.net/releases/';
const fallback = `${base}orange-setup-0.9.2.exe`;
const manifest = {
  schema: 1, channel: 'beta', version: '1.0.0',
  installer_url: `${base}orange-setup-1.0.0.exe`,
  notes: 'Mutual friend requests and synchronized media playback.',
};

async function render(fetch) {
  const links = Array.from({ length: 2 }, () => ({ href: fallback, textContent: 'Get Orange for Windows' }));
  const status = { textContent: '' };
  const notes = { textContent: '' };
  const source = await readFile(new URL('./release.js', import.meta.url), 'utf8');
  await runInNewContext(source, {
    fetch, AbortSignal,
    document: {
      querySelectorAll: () => links,
      getElementById: (id) => id === 'release-status' ? status : notes,
    },
  });
  return { links, status, notes };
}

// A published installer name changes every release; every CTA must follow the
// manifest rather than leaving a second, stale version in the page.
test('every download follows the published version and revalidates the manifest', async () => {
  const result = await render(async (url, options) => {
    assert.equal(url, `${base}orange-beta.json`);
    assert.equal(options.cache, 'no-store');
    assert.ok(options.signal);
    return { ok: true, json: async () => manifest };
  });
  assert.ok(result.links.every((link) => link.href === manifest.installer_url));
  assert.match(result.status.textContent, /1\.0\.0/);
  assert.equal(result.notes.textContent, manifest.notes);
});

// A malformed public response must never turn the site's main action into an
// arbitrary URL or a download from a different release channel.
test('untrusted manifests retain the public Azure installer snapshot', async () => {
  for (const bad of [null, {}, { ...manifest, channel: 'stable' },
    { ...manifest, schema: 2 }, { ...manifest, version: '../file' },
    { ...manifest, installer_url: 'https://example.com/installer.exe' },
    { ...manifest, installer_url: `${manifest.installer_url}?redirect=elsewhere` },
    { ...manifest, installer_url: `${base}orange-setup-0.9.1.exe` }]) {
    const result = await render(async () => ({ ok: true, json: async () => bad }));
    assert.ok(result.links.every((link) => link.href === fallback));
    assert.match(result.status.textContent, /0\.9\.2/);
    assert.doesNotMatch(result.status.textContent, /GitHub/);
  }
});

test('network and HTTP failures leave downloads reachable', async () => {
  for (const fetch of [async () => { throw new Error('Offline'); },
    async () => ({ ok: false }),
    async () => ({ ok: true, json: async () => { throw new Error('Invalid JSON'); } })]) {
    const result = await render(fetch);
    assert.ok(result.links.every((link) => link.href === fallback));
    assert.match(result.status.textContent, /0\.9\.2/);
  }
});
