const repository = 'https://github.com/cristicretu/diri';
export const latestReleaseAPI = 'https://api.github.com/repos/cristicretu/diri/releases/latest';

// Match the desktop packages produced by release.sh and package-linux.sh.
// Helpers, updater metadata, source archives, and incomplete uploads aren't installers.
export function releaseDownloads(release) {
  if (!release || release.draft !== false || release.prerelease !== false
      || !/^v?\d+\.\d+\.\d+$/.test(release.tag_name) || !Array.isArray(release.assets)) return null;
  const version = release.tag_name.replace(/^v/, '');
  const tag = encodeURIComponent(release.tag_name);
  const formats = [
    { name: `diri-${version}-universal.dmg`, label: 'macOS · Universal DMG', primary: true },
    { name: `diri-${version}-universal.zip`, label: 'macOS · Universal ZIP' },
    { name: `diri_${version}_amd64.AppImage`, label: 'Linux beta · x86_64 AppImage' },
    { name: `diri_${version}_amd64.deb`, label: 'Linux beta · x86_64 Debian package' },
  ];
  const downloads = formats.flatMap(format => {
    const url = `${repository}/releases/download/${tag}/${format.name}`;
    const asset = release.assets.find(asset => asset?.name === format.name
      && asset.state === 'uploaded' && Number.isSafeInteger(asset.size) && asset.size > 0
      && asset.browser_download_url === url);
    return asset ? [{ ...format, url }] : [];
  });
  return { version: `v${version}`, notes: `${repository}/releases/tag/${tag}`, downloads };
}

export async function loadLatestRelease(fetchRelease = globalThis.fetch) {
  const controller = new AbortController();
  const timeout = setTimeout(() => controller.abort(), 5000);
  try {
    const response = await fetchRelease(latestReleaseAPI, {
      headers: { Accept: 'application/vnd.github+json' },
      credentials: 'omit',
      signal: controller.signal,
    });
    if (!response.ok) return null;
    return releaseDownloads(await response.json());
  } catch {
    // The initial HTML remains usable without JavaScript, network access, or API quota.
    return null;
  } finally {
    clearTimeout(timeout);
  }
}

export async function initDownloads(document, fetchRelease) {
  const primary = document.querySelector('#download-primary');
  if (!primary) return;
  const release = await loadLatestRelease(fetchRelease);
  if (!release) return;
  const mac = release.downloads.find(asset => asset.primary);
  if (mac) {
    primary.href = mac.url;
    primary.textContent = 'Download for macOS';
    primary.setAttribute('aria-label', `Download Diri ${release.version} for macOS, universal DMG`);
  }
  document.querySelector('#download-version').textContent = mac
    ? `${release.version} · macOS 15+` : release.version;
  document.querySelector('#download-notes').href = release.notes;
  const links = release.downloads.filter(asset => !asset.primary).map(asset => {
    const link = document.createElement('a');
    link.href = asset.url;
    link.textContent = asset.label;
    return link;
  });
  document.querySelector('#download-assets').replaceChildren(...links);
}

if (typeof document !== 'undefined') void initDownloads(document);
