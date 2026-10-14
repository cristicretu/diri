// Generates every data-driven page on diri.sh: docs, agents, comparisons, releases,
// plus llms.txt, the sitemap and the list of share cards. Output is committed, so the
// site previews with no build step. `--check` fails when anything is stale.
import { createHash } from 'node:crypto';
import { readFile, writeFile, mkdir, rm } from 'node:fs/promises';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { root, site, repo, today, ogKey } from './render.mjs';
import { buildDocs } from './docs.mjs';
import { buildPages } from './pages.mjs';

// Bump when scripts/og.mjs draws cards differently, so every card is redrawn.
export const CARD_DESIGN = 1;
export const cardHash = card => createHash('sha256').update(JSON.stringify([CARD_DESIGN, card])).digest('hex').slice(0, 16);

export async function buildSite() {
  const docs = await buildDocs();
  const pages = await buildPages();
  const outputs = new Map([...docs.outputs, ...pages.outputs]);

  const llms = [`# Diri\n\n> Diri is a native desktop app for running many coding agents (Claude Code, Codex, Cursor, Gemini and 16 more) side by side, with live status, git worktrees, review, notes, remote SSH hosts, and an MCP server that lets agents start and coordinate other agents. Free and open source (AGPL-3.0).\n\nEach page is available as Markdown at the URL below. The docs are concatenated in ${site}/llms-full.txt. A read-only docs MCP server is at ${site}/mcp.\n`, ...docs.llms];
  for (const group of ['Agents', 'Compare', 'Releases']) {
    const list = pages.entries.filter(entry => entry.group === group);
    if (!list.length) continue;
    llms.push(`\n## ${group}\n`, ...list.map(entry => `- [${entry.title}](${site}${entry.md || entry.url}): ${entry.description}`));
  }
  outputs.set('llms.txt', llms.join('\n') + `\n\n## Optional\n\n- [Guides](${site}/guides/): Beginner walkthroughs with screenshots.\n- [Source code](${repo}): AGPL-3.0.\n`);
  outputs.set('llms-full.txt', `# Diri documentation\n${docs.full}\n`);

  // Sitemap: hand-written pages stay as listed; generated sections are rebuilt.
  const generated = /\/(docs|agents|compare|whats-new)\//;
  const sitemap = await readFile(resolve(root, 'sitemap.xml'), 'utf8');
  const kept = sitemap.split('\n').filter(line => line.includes('<url>') && !generated.test(line));
  const fresh = [...docs.urls, ...pages.urls].map(url => `  <url><loc>${site}${url}</loc><lastmod>${today}</lastmod></url>`);
  outputs.set('sitemap.xml', `<?xml version="1.0" encoding="UTF-8"?>\n<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">\n${[...kept, ...fresh].join('\n')}\n</urlset>\n`);

  const cards = [...docs.cards, ...pages.cards].map(card => ({ key: ogKey(card.url), ...card }));
  outputs.set('og/cards.json', JSON.stringify(cards.map(card => ({ ...card, hash: cardHash(card) })), null, 2) + '\n');

  const htmlPages = [...outputs.keys()].filter(file => file.endsWith('.html'));
  return { outputs, htmlPages, cards };
}

export const sitePages = async () => (await buildSite()).htmlPages;

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const { outputs, cards } = await buildSite();
  if (process.argv.includes('--check')) {
    const stale = [];
    for (const [file, content] of outputs) if (await readFile(resolve(root, file), 'utf8').catch(() => null) !== content) stale.push(file);
    if (stale.length) { console.error(`Generated pages are stale, run \`npm run docs\`:\n  ${stale.join('\n  ')}`); process.exit(1); }
    const rendered = JSON.parse(await readFile(resolve(root, 'og/rendered.json'), 'utf8').catch(() => '{}'));
    const undrawn = cards.filter(card => rendered[card.key] !== cardHash(card)).map(card => card.key);
    if (undrawn.length) { console.error(`Share cards are stale, run \`npm run og\` (needs Chrome):\n  ${undrawn.join('\n  ')}`); process.exit(1); }
    console.log(`Generated pages up to date (${outputs.size} files, ${cards.length} share cards).`);
  } else {
    for (const dir of ['docs', 'agents', 'compare']) await rm(resolve(root, dir), { recursive: true, force: true });
    for (const [file, content] of outputs) {
      await mkdir(dirname(resolve(root, file)), { recursive: true });
      await writeFile(resolve(root, file), content);
    }
    console.log(`Wrote ${outputs.size} files. Run \`npm run og\` if share cards changed.`);
  }
}
