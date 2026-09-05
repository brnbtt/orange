(async () => {
  const base = 'https://orangealpha0d8d5893e69a3.blob.core.windows.net/releases/';
  const status = document.getElementById('release-status');
  try {
    const response = await fetch(`${base}orange-beta.json`, {
      cache: 'no-store',
      signal: AbortSignal.timeout(8000),
    });
    if (!response.ok) throw new Error('Release unavailable');
    const release = await response.json();
    // The manifest is also used by installed clients. Only the exact published
    // Windows asset is a download destination, never an arbitrary response URL.
    if (!release || release.schema !== 1 || release.channel !== 'beta' ||
        typeof release.version !== 'string' || !/^\d+\.\d+\.\d+$/.test(release.version) ||
        release.installer_url !== `${base}orange-setup-${release.version}.exe`) {
      throw new Error('Invalid release');
    }
    for (const link of document.querySelectorAll('[data-download]')) {
      link.href = release.installer_url;
      link.textContent = 'Download for Windows';
    }
    status.textContent = `Version ${release.version} · Beta · Windows 10 / 11 · 64-bit`;
    if (typeof release.notes === 'string') {
      document.getElementById('release-notes').textContent = release.notes;
    }
  } catch {
    // The HTML already links to all releases, including prereleases. GitHub's
    // /latest endpoint would skip the beta channel we actually publish.
    status.textContent = 'Windows 10 / 11 · 64-bit · Get the latest beta on GitHub.';
  }
})();
