// Builds diri.sh/docs from docs-src/*.md: one static page per doc, raw Markdown
// beside it for agents, a search index, and llms.txt. Run through scripts/site.mjs.
import { readFile, readdir } from 'node:fs/promises';
import { resolve } from 'node:path';
import { root, site, repo, today, esc, slugify, plain, inline, render, frontmatter, header, footer, ogPath } from './render.mjs';

const source = resolve(root, 'docs-src');
const pageUrl = slug => slug === 'index' ? '/docs/' : `/docs/${slug}/`;

// The sidebar. Every docs-src page must appear here exactly once.
export const NAV = [
  { group: 'Getting started', pages: [
    ['index', 'file'], ['install', 'download'], ['quickstart', 'new-agent'], ['keyboard-shortcuts', 'keyboard'],
  ] },
  { group: 'Working with agents', pages: [
    ['sessions', 'terminal'], ['worktrees', 'branch'], ['notes', 'note'], ['scheduled-tasks', 'clock'], ['accounts', 'user'],
  ] },
  { group: 'Automation', pages: [
    ['mcp', 'plug'], ['mcp-tools', 'code'], ['cli', 'terminal'], ['agents', 'apps'],
  ] },
  { group: 'Remote', pages: [
    ['remote-hosts', 'server'],
  ] },
  { group: 'Reference', pages: [
    ['security', 'shield'], ['troubleshooting', 'lifebuoy'],
  ] },
];

// ── The MCP tool reference is generated from the Rust catalog ───────────────
const TOOL_GROUPS = [
  ['Start and manage agents', ['spawn_agent', 'spawn_agents', 'fork_agent', 'manage_agent', 'release_agent', 'list_agents', 'get_status']],
  ['Talk to agents and wait', ['send_prompt', 'wait_any', 'wait_for_agent', 'read_output']],
  ['Tracked tasks', ['submit_task', 'submit_tasks', 'get_task', 'wait_for_task', 'report_task', 'answer_task', 'cancel_task', 'list_tasks']],
  ['Split panes', ['get_layout', 'manage_layout']],
  ['Lineage', ['whoami', 'list_children', 'wait_for_children', 'summarize_children', 'report_to_parent']],
  ['Code and worktrees', ['get_diff', 'integrate', 'create_worktree', 'list_worktrees', 'remove_worktree', 'get_artifacts', 'quick_open_include']],
  ['Schedules', ['schedule_agent', 'list_schedules', 'delete_schedule']],
  ['Browser', ['browser', 'test_run']],
  ['Notes', ['list_notes', 'read_note', 'write_note', 'edit_note', 'replace_section', 'create_note', 'start_from_note', 'note_history']],
];

function schemaType(schema = {}) {
  if (schema.enum?.includes('shell') && schema.enum.length > 6) return `agent label or \`shell\` ([list](/docs/agents/))`;
  if (schema.enum) return schema.enum.map(v => `\`${v}\``).join(', ');
  if (schema.type === 'array') return `${schemaType(schema.items)} array`.replace(/^object array$/, 'array of objects');
  return schema.type || 'any';
}

function toolMarkdown(catalog) {
  const byName = new Map(catalog.tools.map(tool => [tool.name, tool]));
  const placed = new Set();
  const groups = TOOL_GROUPS.map(([name, list]) => [name, list.filter(n => byName.has(n))]);
  for (const [, list] of groups) list.forEach(n => placed.add(n));
  const rest = catalog.tools.map(t => t.name).filter(n => !placed.has(n));
  if (rest.length) groups.push(['Other', rest]);
  let md = '';
  for (const [group, names] of groups) {
    if (!names.length) continue;
    md += `\n## ${group}\n\n`;
    for (const name of names) {
      const tool = byName.get(name);
      md += `### \`${name}\`\n\n${tool.description.replace(/\|/g, '/')}\n\n`;
      const props = Object.entries(tool.inputSchema?.properties || {});
      const required = new Set(tool.inputSchema?.required || []);
      if (!props.length) { md += 'No arguments.\n\n'; continue; }
      md += '| Argument | Type | Notes |\n| --- | --- | --- |\n';
      props.sort(([a], [b]) => Number(required.has(b)) - Number(required.has(a)));
      for (const [arg, schema] of props) {
        const notes = [required.has(arg) ? '**Required.**' : '', schema.description || '', schema.default !== undefined ? `Default \`${JSON.stringify(schema.default)}\`.` : '',
          schema.items?.properties ? `Each item: ${Object.keys(schema.items.properties).map(k => `\`${k}\``).join(', ')}.` : ''].filter(Boolean).join(' ');
        md += `| \`${arg}\` | ${schemaType(schema)} | ${notes.replace(/\|/g, '/')} |\n`;
      }
      md += '\n';
    }
  }
  return md;
}

function sidebar(current, pages) {
  const groups = NAV.map(({ group, pages: list }) => `<div class="docs-group"><div class="docs-group-head"><span class="icon" data-icon="chevron-down" aria-hidden="true"></span>${esc(group)}</div><ul>${list.map(([slug, icon]) => {
    const page = pages.get(slug);
    if (!page) return '';
    return `<li><a class="docs-row" href="${pageUrl(slug)}"${slug === current ? ' aria-current="page"' : ''}><span class="icon" data-icon="${icon}" aria-hidden="true"></span><span>${esc(page.meta.nav || page.meta.title)}</span></a></li>`;
  }).join('')}</ul></div>`).join('');
  return `<nav class="docs-sidebar" id="docs-sidebar" aria-label="Documentation">
          <button class="docs-search" type="button" data-open-search><span class="icon" data-icon="search" aria-hidden="true"></span><span>Search docs</span><kbd>⌘K</kbd></button>
          ${groups}
          <div class="docs-sidebar-foot"><a href="/llms.txt"><span class="icon" data-icon="markdown" aria-hidden="true"></span>llms.txt</a><a href="/docs/mcp/#docs-mcp"><span class="icon" data-icon="plug" aria-hidden="true"></span>Docs MCP</a></div>
        </nav>`;
}

function template({ slug, meta, html, toc, group, prev, next, pages }) {
  const url = site + pageUrl(slug);
  const title = slug === 'index' ? 'Documentation · Diri' : `${meta.title} · Diri docs`;
  const ld = JSON.stringify({ '@context': 'https://schema.org', '@type': 'TechArticle', headline: meta.title, description: meta.description, url, inLanguage: 'en', dateModified: today, publisher: { '@type': 'Organization', name: 'Diri', url: site + '/' }, isPartOf: { '@type': 'WebSite', name: 'Diri', url: site + '/' } });
  const raw = slug === 'index' ? '/docs/index.md' : `/docs/${slug}.md`;
  const ask = `https://claude.ai/new?q=${encodeURIComponent(`Read ${site}${raw} and help me with diri.`)}`;
  const tocHtml = toc.length > 1 ? `<span>On this page</span>${toc.map(t => `<a href="#${t.id}">${esc(t.text)}</a>`).join('')}` : '';
  const pager = `<nav class="docs-pager" aria-label="Previous and next">${prev ? `<a class="prev" href="${pageUrl(prev)}"><span>Previous</span>${esc(pages.get(prev).meta.title)}</a>` : '<span></span>'}${next ? `<a class="next" href="${pageUrl(next)}"><span>Next</span>${esc(pages.get(next).meta.title)}</a>` : '<span></span>'}</nav>`;
  return `<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <meta name="theme-color" content="#191724">
  <title>${esc(title)}</title>
  <meta name="description" content="${esc(meta.description)}">
  <meta name="robots" content="index, follow, max-image-preview:large">
  <link rel="canonical" href="${url}">
  <link rel="alternate" type="text/markdown" href="${raw}">
  <meta property="og:type" content="article">
  <meta property="og:site_name" content="Diri">
  <meta property="og:title" content="${esc(meta.title)}">
  <meta property="og:description" content="${esc(meta.description)}">
  <meta property="og:url" content="${url}">
  <meta property="og:image" content="${site}${ogPath(pageUrl(slug))}">
  <meta property="og:image:width" content="1200">
  <meta property="og:image:height" content="630">
  <meta property="og:image:alt" content="${esc(meta.title)}: diri documentation">
  <meta name="twitter:card" content="summary_large_image">
  <link rel="icon" href="/favicon.svg" type="image/svg+xml">
  <link rel="stylesheet" href="/style.css">
  <link rel="stylesheet" href="/guides.css">
  <link rel="stylesheet" href="/docs.css">
  <script src="/docs.js" defer></script>
  <script type="application/ld+json">${ld}</script>
  <script type="module" src="/stats.js"></script>
</head>
<body class="guide-page docs-page">
  <a class="skip-link" href="#main">Skip to content</a>
  <div class="site-wrap">
    ${header('Docs')}
    <div class="docs-shell">
      <button class="docs-menu" type="button" aria-expanded="false" aria-controls="docs-sidebar"><span class="icon" data-icon="sidebar" aria-hidden="true"></span>${esc(group)}<span class="docs-menu-sep">/</span><strong>${esc(meta.nav || meta.title)}</strong></button>
      ${sidebar(slug, pages)}
      <main id="main" class="docs-main" data-raw="${raw}">
        <header class="docs-heading"><span class="eyebrow">${esc(group)}</span><h1>${esc(meta.title)}</h1><p>${inline(meta.lead || meta.description)}</p></header>
        <article class="docs-content">
${html}
        </article>
        ${pager}
      </main>
      <aside class="docs-rail" aria-label="On this page">
        <div class="docs-rail-inner">${tocHtml ? `<nav class="docs-toc">${tocHtml}</nav>` : ''}
          <div class="docs-actions"><button type="button" data-copy-page><span class="icon" data-icon="copy" aria-hidden="true"></span><span>Copy as Markdown</span></button><a href="${raw}"><span class="icon" data-icon="markdown" aria-hidden="true"></span>View Markdown</a><a href="${ask}" rel="noopener"><span class="icon" data-icon="external-link" aria-hidden="true"></span>Ask Claude</a><a href="${repo}/edit/main/website/docs-src/${slug}.md" rel="noopener"><span class="icon" data-icon="github" aria-hidden="true"></span>Edit on GitHub</a></div>
        </div>
      </aside>
    </div>
    ${footer}
  </div>
  <div class="docs-search-dialog" hidden>
    <div class="docs-search-scrim" data-close-search></div>
    <div class="docs-search-panel" role="dialog" aria-modal="true" aria-label="Search documentation">
      <label class="docs-search-input"><span class="icon" data-icon="search" aria-hidden="true"></span><input type="search" placeholder="Search the docs" autocomplete="off" spellcheck="false" aria-controls="docs-search-results"><kbd>esc</kbd></label>
      <ul id="docs-search-results" class="docs-search-results" role="listbox"></ul>
      <div class="docs-search-foot"><span><kbd>↑</kbd><kbd>↓</kbd> to move</span><span><kbd>↵</kbd> to open</span></div>
    </div>
  </div>
</body>
</html>
`;
}

// ── Build ───────────────────────────────────────────────────────────────────
export async function buildDocs() {
  const files = (await readdir(source)).filter(f => f.endsWith('.md'));
  const pages = new Map();
  for (const file of files) {
    const slug = file.replace(/\.md$/, '');
    const { meta, body } = frontmatter(await readFile(resolve(source, file), 'utf8'), file);
    let markdown = body;
    if (markdown.includes('{{mcp-tools}}')) {
      const catalog = JSON.parse(await readFile(resolve(source, 'mcp-tools.json'), 'utf8'));
      markdown = markdown.replace('{{mcp-tools}}', toolMarkdown(catalog).trim());
    }
    pages.set(slug, { meta, markdown });
  }
  // DOCS_DRAFT=1 previews while some pages are still being written.
  const order = NAV.flatMap(({ group, pages: list }) => list.map(([slug]) => ({ slug, group }))).filter(({ slug }) => !process.env.DOCS_DRAFT || pages.has(slug));
  for (const { slug } of order) if (!pages.has(slug)) throw new Error(`docs-src/${slug}.md is in NAV but missing`);
  for (const slug of pages.keys()) if (!order.some(o => o.slug === slug)) throw new Error(`docs-src/${slug}.md is not in NAV`);

  const outputs = new Map();
  const search = [];
  const llms = [];
  let full = '';
  let lastGroup = '';
  for (const [n, { slug, group }] of order.entries()) {
    const { meta, markdown } = pages.get(slug);
    const { html, toc } = await render(markdown);
    const prev = order[n - 1]?.slug;
    const next = order[n + 1]?.slug;
    const path = slug === 'index' ? 'docs/index.html' : `docs/${slug}/index.html`;
    outputs.set(path, template({ slug, meta, html, toc, group, prev, next, pages }));
    const rawMd = `# ${meta.title}\n\n> ${meta.description}\n\n${markdown}`;
    outputs.set(slug === 'index' ? 'docs/index.md' : `docs/${slug}.md`, rawMd);
    full += `\n\n---\n\nSource: ${site}${pageUrl(slug)}\n\n${rawMd}`;
    if (group !== lastGroup) { llms.push(`\n## ${group}\n`); lastGroup = group; }
    llms.push(`- [${meta.title}](${site}${slug === 'index' ? '/docs/index.md' : `/docs/${slug}.md`}): ${meta.description}`);

    // Search: the page itself, then each section with its first words.
    search.push({ t: meta.title, g: group, u: pageUrl(slug), x: meta.description });
    const sections = markdown.split(/^(?=#{2,3} )/m);
    for (const section of sections) {
      const head = section.match(/^(#{2,3}) (.+)$/m);
      if (!head || section.indexOf(head[0]) !== 0) continue;
      const body = plain(section.slice(head[0].length).replace(/```[\s\S]*?```/g, ' ').replace(/^\|.*\n\|[\s:|-]+\|\s*$/gm, '').replace(/^\|.*$/gm, m => m.replace(/\|/g, ' ')).replace(/^> \[!\w+\]/gm, ''));
      const id = slugify(head[2]);
      search.push({ t: plain(head[2]), p: meta.title, g: group, u: `${pageUrl(slug)}#${id}`, x: body.slice(0, 220) });
    }
  }
  outputs.set('docs/search.json', JSON.stringify(search) + '\n');
  const cards = order.map(({ slug, group }) => ({ url: pageUrl(slug), eyebrow: slug === 'index' ? 'Documentation' : `Docs · ${group}`, title: pages.get(slug).meta.title }));
  return { outputs, order, llms, full, cards, urls: order.map(({ slug }) => pageUrl(slug)) };
}

