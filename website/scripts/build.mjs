import { cp, mkdir, readFile, readdir, rm, writeFile } from 'node:fs/promises';
import { createHash } from 'node:crypto';
import { dirname, resolve, extname, basename } from 'node:path';
import { fileURLToPath } from 'node:url';
import { sitePages } from './site.mjs';
const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const output = resolve(root, 'dist');
await rm(output, { recursive: true, force: true });
await mkdir(output, { recursive: true });
const pages = new Map(await Promise.all(['index.html', '404.html', 'guides/index.html', 'guides/first-agent/index.html', 'guides/never-lose-work/index.html', 'guides/parallel-agents/index.html', 'guides/agent-teams/index.html', 'guides/review-changes/index.html', 'guides/scheduled-tasks/index.html', 'guides/remote-sessions/index.html', 'guides/shortcuts/index.html', 'whats-new/index.html', ...await sitePages()].map(async file => [file, await readFile(resolve(root, file), 'utf8')])));
const hashed = [];
for (const file of ['style.css', 'guides.css', 'app.js', 'downloads.js', 'guides.js', 'stats.js', 'docs.css', 'docs.js']) {
  const content = await readFile(resolve(root, file));
  const hash = createHash('sha256').update(content).digest('hex').slice(0, 12);
  const ext = extname(file);
  const name = `${basename(file, ext)}.${hash}${ext}`;
  hashed.push(name);
  await writeFile(resolve(output, name), content);
  for (const [page, html] of pages) pages.set(page, html.replaceAll(file, name));
}
for (const [file, content] of pages) {
  await mkdir(dirname(resolve(output, file)), { recursive: true });
  await writeFile(resolve(output, file), content);
}
for (const file of ['robots.txt', 'sitemap.xml', '_redirects', 'favicon.svg', 'favicon-96.png', 'apple-touch-icon.png', 'assets', 'llms.txt', 'llms-full.txt', 'docs-search.js']) {
  await cp(resolve(root, file), resolve(output, file), { recursive: true });
}
// Agents read the raw Markdown and the search index beside each page.
for (const dir of ['docs', 'agents', 'compare']) {
  for (const file of await readdir(resolve(root, dir), { recursive: true }).catch(() => [])) {
    if (/\.(md|json)$/.test(file)) await cp(resolve(root, dir, file), resolve(output, dir, file));
  }
}
// Share cards: the images only, not the bookkeeping beside them.
await mkdir(resolve(output, 'og'), { recursive: true });
for (const file of await readdir(resolve(root, 'og'))) if (file.endsWith('.jpg')) await cp(resolve(root, 'og', file), resolve(output, 'og', file));
const jsonHashes = [...new Set([...pages.values()].flatMap(html =>
  [...html.matchAll(/<script type="application\/ld\+json">([\s\S]*?)<\/script>/g)]
    .map(match => `'sha256-${createHash('sha256').update(match[1]).digest('base64')}'`)
))].join(' ');
const headers = `/*
  X-Content-Type-Options: nosniff
  Referrer-Policy: strict-origin-when-cross-origin
  Content-Security-Policy: default-src 'self'; script-src 'self' ${jsonHashes}; style-src 'self' 'unsafe-inline'; img-src 'self' data:; font-src 'self'; connect-src 'self' https://api.github.com; object-src 'none'; base-uri 'self'; frame-ancestors 'none'; form-action 'none'

https://:project.pages.dev/*
  X-Robots-Tag: noindex

https://:version.:project.pages.dev/*
  X-Robots-Tag: noindex

${hashed.map(file => `/${file}\n  Cache-Control: public, max-age=31536000, immutable`).join('\n\n')}
`;
await writeFile(resolve(output, '_headers'), headers);
console.log('Built website/dist for Cloudflare Pages (static assets only).');
