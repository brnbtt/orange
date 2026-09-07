(async () => {
  const base = 'https://orangealpha0d8d5893e69a3.blob.core.windows.net/releases/';
  const status = document.getElementById('release-status');
  const links = document.querySelectorAll('[data-download]');
  const fallbackVersion = links[0].href.match(/orange-setup-(\d+\.\d+\.\d+)\.exe$/)?.[1];
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
    const copy = window.orangeCopy || {};
    const i18n = window.orangeI18n;
    const fill = i18n?.fill || ((template, value) => String(template).replace('{}', value));
    for (const link of links) {
      link.href = release.installer_url;
      link.textContent = copy.download?.button || 'Download for Windows';
    }
    status.textContent = fill(
      copy.download?.versionLine || 'Version {} · Windows 10 / 11 · 64-bit',
      release.version,
    );
    if (typeof release.notes === 'string') {
      document.getElementById('release-notes').textContent = release.notes;
    }
  } catch {
    // The repository is private. Deploy bakes a public, immutable installer
    // into the HTML so a failed check still leaves a usable download.
    const copy = window.orangeCopy || {};
    const i18n = window.orangeI18n;
    const fill = i18n?.fill || ((template, value) => String(template).replace('{}', value));
    status.textContent = fill(
      copy.download?.updateUnavailable || 'Update check unavailable. Download {} above, or reload to check again.',
      fallbackVersion || 'the saved release',
    );
  }
})();
