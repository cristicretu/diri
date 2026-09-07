import assert from 'node:assert/strict';
import { test } from 'node:test';
import { latestReleaseAPI, loadLatestRelease, releaseDownloads } from '../downloads.js';

function fixture(version = '0.6.2') {
  return {
    tag_name: `v${version}`, draft: false, prerelease: false,
    assets: [
      `diri-${version}-universal.dmg`, `diri-${version}-universal.zip`,
      `diri_${version}_amd64.AppImage`, `diri_${version}_amd64.deb`,
      'appcast.json', 'SHA256SUMS', 'diri-remote-linux-x86_64',
    ].map(name => ({
      name, state: 'uploaded', size: 1024,
      browser_download_url: `https://github.com/cristicretu/diri/releases/download/v${version}/${name}`,
    })),
  };
}

test('resolves macOS and Linux installers, excluding helpers and metadata', () => {
  const result = releaseDownloads(fixture());
  assert.equal(result.version, 'v0.6.2');
  assert.equal(result.notes, 'https://github.com/cristicretu/diri/releases/tag/v0.6.2');
  assert.deepEqual(result.downloads.map(asset => asset.label), [
    'macOS · Universal DMG', 'macOS · Universal ZIP',
    'Linux beta · x86_64 AppImage', 'Linux beta · x86_64 Debian package',
  ]);
  assert.equal(result.downloads.find(asset => asset.primary).name, 'diri-0.6.2-universal.dmg');
});

test('follows a new release without a hardcoded version or download URL', () => {
  const result = releaseDownloads(fixture('1.20.3'));
  assert.equal(result.version, 'v1.20.3');
  assert.ok(result.downloads.every(asset => asset.url.includes('/v1.20.3/')));
});

test('only offers packages present in this release', () => {
  const release = fixture();
  release.assets = release.assets.slice(0, 2);
  assert.equal(releaseDownloads(release).downloads.length, 2);
  release.assets = release.assets.slice(1);
  assert.equal(releaseDownloads(release).downloads.some(asset => asset.primary), false);
  release.assets = [];
  assert.deepEqual(releaseDownloads(release).downloads, []);
});

test('rejects draft, prerelease, and malformed release data', () => {
  for (const release of [null, {}, [], { ...fixture(), draft: true },
    { ...fixture(), prerelease: true }, { ...fixture(), tag_name: 'v1.0.0-beta.1' },
    { ...fixture(), tag_name: '../main' }, { ...fixture(), assets: null }]) {
    assert.equal(releaseDownloads(release), null);
  }
});

test('rejects incomplete, empty, mismatched, or untrusted asset URLs', () => {
  for (const change of [
    { state: 'new' }, { size: 0 }, { size: -1 },
    { browser_download_url: 'https://example.com/diri-0.6.2-universal.dmg' },
    { browser_download_url: 'javascript:alert(1)' },
    { browser_download_url: 'https://github.com/other/diri/releases/download/v0.6.2/diri-0.6.2-universal.dmg' },
    { browser_download_url: 'https://github.com/cristicretu/diri/releases/download/v0.6.1/diri-0.6.2-universal.dmg' },
  ]) {
    const release = fixture();
    release.assets = [null, { ...release.assets[0], ...change }];
    assert.deepEqual(releaseDownloads(release).downloads, []);
  }
});

test('requests the public latest release without credentials', async () => {
  const result = await loadLatestRelease(async (url, options) => {
    assert.equal(url, latestReleaseAPI);
    assert.equal(options.credentials, 'omit');
    assert.equal(options.headers.Authorization, undefined);
    assert.ok(options.signal instanceof AbortSignal);
    return Response.json(fixture());
  });
  assert.equal(result.version, 'v0.6.2');
});

test('preserves the HTML fallback for API errors, invalid JSON, and network failure', async () => {
  for (const status of [403, 404, 429, 500]) {
    assert.equal(await loadLatestRelease(async () => new Response('', { status })), null);
  }
  assert.equal(await loadLatestRelease(async () => new Response('not json')), null);
  assert.equal(await loadLatestRelease(async () => { throw new TypeError('offline'); }), null);
});

test('aborts a stalled lookup after five seconds', async t => {
  t.mock.timers.enable({ apis: ['setTimeout'] });
  let signal;
  const pending = loadLatestRelease(async (_url, options) => new Promise((_resolve, reject) => {
    signal = options.signal;
    signal.addEventListener('abort', () => reject(signal.reason), { once: true });
  }));
  t.mock.timers.tick(5000);
  assert.equal(await pending, null);
  assert.equal(signal.aborted, true);
});
