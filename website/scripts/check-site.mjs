import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFile, readdir, stat } from 'node:fs/promises';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
const root = resolve(dirname(fileURLToPath(import.meta.url)), '../dist');
const read = file => readFile(resolve(root, file), 'utf8');
const html = await read('index.html');
assert.match(html, /<html lang="en">/);
assert.equal((html.match(/<h1\b/g) || []).length, 1);
assert.match(html, /rel="canonical" href="https:\/\/diri\.sh\/"/);
assert.match(html, /name="description" content="[^"]{80,180}"/);
assert.match(html, /property="og:image" content="https:\/\/diri\.sh\/assets\/social-card\.jpg"/);
assert.match(html, /name="twitter:card" content="summary_large_image"/);
assert.doesNotMatch(html, /content="noindex/);
const data = JSON.parse(html.match(/<script type="application\/ld\+json">([\s\S]*?)<\/script>/)[1]);
assert.equal(data['@context'], 'https://schema.org');
assert.ok(data['@graph'].some(item => item['@type'] === 'WebSite' && item.url === 'https://diri.sh/'));
assert.ok(data['@graph'].some(item => item['@type'] === 'SoftwareApplication' && item.name === 'Diri'));
assert.ok((html.match(/<dt>/g) || []).length >= 20, 'Feature copy must be in the initial HTML');
const robots = await read('robots.txt');
assert.match(robots, /Allow: \//);
assert.match(robots, /Sitemap: https:\/\/diri\.sh\/sitemap\.xml/);
assert.match(await read('sitemap.xml'), /<loc>https:\/\/diri\.sh\/<\/loc>/);
assert.match(await read('404.html'), /content="noindex, follow"/);
assert.match(await read('_redirects'), /\/index\.html \/ 301/);
const headers = await read('_headers');
assert.match(headers, /https:\/\/:project\.pages\.dev\/\*\n  X-Robots-Tag: noindex/);
assert.match(headers, /https:\/\/:version\.:project\.pages\.dev\/\*\n  X-Robots-Tag: noindex/);
const pagePaths = ['index.html', '404.html', 'guides/index.html', 'guides/parallel-agents/index.html', 'guides/remote-sessions/index.html'];
const sitemap = await read('sitemap.xml');
const titles = new Set();
for (const page of pagePaths) {
  const content = await read(page);
  assert.equal((content.match(/<h1\b/g) || []).length, 1, `${page}: one heading`);
  if (page.startsWith('guides/')) {
    const url = 'https://diri.sh/' + page.replace(/index\.html$/, '');
    assert.ok(content.includes(`rel="canonical" href="${url}"`), `${page}: canonical`);
    assert.ok(sitemap.includes(`<loc>${url}</loc>`), `${page}: sitemap`);
    assert.match(content, /name="description" content="[^"]{80,180}"/);
    assert.match(content, /name="twitter:card" content="summary_large_image"/);
    assert.doesNotMatch(content, /content="noindex/);
    const title = content.match(/<title>(.*?)<\/title>/)[1];
    assert.ok(!titles.has(title), `${page}: unique title`);
    titles.add(title);
  }
  for (const match of content.matchAll(/<script type="application\/ld\+json">([\s\S]*?)<\/script>/g)) {
    assert.equal(JSON.parse(match[1])['@context'], 'https://schema.org');
    const hash = createHash('sha256').update(match[1]).digest('base64');
    assert.ok(headers.includes(`'sha256-${hash}'`), `${page}: structured data allowed by CSP`);
  }
  for (const match of content.matchAll(/(?:src|href)="([^"]+)"/g)) {
    const target = match[1];
    if (/^[a-z]+:/.test(target)) continue;
    const [path, fragment] = target.split('#');
    let file = path ? resolve(path.startsWith('/') ? root : dirname(resolve(root, page)), path.replace(/^\//, '')) : resolve(root, page);
    if ((await stat(file)).isDirectory()) file = resolve(file, 'index.html');
    await stat(file);
    if (fragment && file.endsWith('.html')) {
      const linked = await readFile(file, 'utf8');
      assert.ok(linked.includes(`id="${fragment}"`), `${page}: missing anchor ${target}`);
    }
  }
}
for (const entry of await readdir(root)) assert.ok(!['README.md','CLOUDFLARE.md','package.json','server.mjs','scripts','.wrangler','node_modules'].includes(entry), `Source file leaked into build: ${entry}`);
assert.ok((await stat(resolve(root, 'assets/social-card.jpg'))).size < 300_000);
console.log('SEO/build checks passed: canonical metadata, schema, crawl files, preview exclusion, local assets, and public output.');
