import { cp, mkdir, readFile, rm, writeFile } from 'node:fs/promises';
import { createHash } from 'node:crypto';
import { dirname, resolve, extname, basename } from 'node:path';
import { fileURLToPath } from 'node:url';
const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const output = resolve(root, 'dist');
await rm(output, { recursive: true, force: true });
await mkdir(output, { recursive: true });
const pages = new Map(await Promise.all(['index.html', '404.html'].map(async file => [file, await readFile(resolve(root, file), 'utf8')])));
const hashed = [];
for (const file of ['style.css', 'app.js', 'agent-previews.js', 'mesh.js']) {
  const content = await readFile(resolve(root, file));
  const hash = createHash('sha256').update(content).digest('hex').slice(0, 12);
  const ext = extname(file);
  const name = `${basename(file, ext)}.${hash}${ext}`;
  hashed.push(name);
  await writeFile(resolve(output, name), content);
  for (const [page, html] of pages) pages.set(page, html.replaceAll(file, name));
}
for (const [file, content] of pages) await writeFile(resolve(output, file), content);
for (const file of ['robots.txt', 'sitemap.xml', '_redirects', 'favicon.svg', 'favicon-96.png', 'apple-touch-icon.png', 'assets']) {
  await cp(resolve(root, file), resolve(output, file), { recursive: true });
}
const structuredData = pages.get('index.html').match(/<script type="application\/ld\+json">([\s\S]*?)<\/script>/)[1];
const jsonHash = createHash('sha256').update(structuredData).digest('base64');
const headers = `/*
  X-Content-Type-Options: nosniff
  Referrer-Policy: strict-origin-when-cross-origin
  Content-Security-Policy: default-src 'self'; script-src 'self' 'sha256-${jsonHash}'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; font-src 'self'; connect-src 'self'; object-src 'none'; base-uri 'self'; frame-ancestors 'none'; form-action 'none'

https://:project.pages.dev/*
  X-Robots-Tag: noindex

https://:version.:project.pages.dev/*
  X-Robots-Tag: noindex

${hashed.map(file => `/${file}\n  Cache-Control: public, max-age=31536000, immutable`).join('\n\n')}
`;
await writeFile(resolve(output, '_headers'), headers);
console.log('Built website/dist for Cloudflare Pages (static assets only).');
