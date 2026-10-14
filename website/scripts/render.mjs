// Shared by every generated page: a small Markdown renderer and the site chrome.
import { readFile } from 'node:fs/promises';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

export const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
export const site = 'https://diri.sh';
export const repo = 'https://github.com/cristicretu/diri';
export const today = '2026-10-01';

export const esc = text => text.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;');
export const slugify = text => text.toLowerCase().replace(/<[^>]+>/g, '').replace(/[`*]/g, '').replace(/&[a-z]+;/g, '').replace(/[^a-z0-9]+/g, '-').replace(/^-|-$/g, '');
export const plain = md => md.replace(/<\/?kbd>/g, '').replace(/!\[[^\]]*\]\([^)]*\)/g, '').replace(/\[([^\]]+)\]\([^)]*\)/g, '$1').replace(/[`*]/g, '').replace(/\s+/g, ' ').trim();

// ── Inline Markdown ─────────────────────────────────────────────────────────
export function inline(text) {
  const held = [];
  const hold = html => `\u0000${held.push(html) - 1}\u0000`;
  text = text.replace(/`([^`]+)`/g, (_, code) => hold(`<code>${esc(code)}</code>`));
  text = text.replace(/<kbd>(.*?)<\/kbd>/g, (_, key) => hold(`<kbd>${esc(key)}</kbd>`));
  text = esc(text);
  text = text.replace(/\[([^\]]+)\]\(([^)\s]+)\)/g, (_, label, href) => {
    const external = /^https?:/.test(href) && !href.startsWith(site);
    return hold(`<a href="${href}"${external ? ' rel="noopener"' : ''}>${label}</a>`);
  });
  text = text.replace(/\*\*([^*]+)\*\*/g, '<strong>$1</strong>');
  text = text.replace(/(^|[\s(])\*([^*\s][^*]*?)\*(?=[\s).,;:!?]|$)/g, '$1<em>$2</em>');
  return text.replace(/\u0000(\d+)\u0000/g, (_, i) => held[i]).replace(/\u0000(\d+)\u0000/g, (_, i) => held[i]);
}

// ── Code: a few colours for the languages the docs use ──────────────────────
function highlight(code, lang) {
  if (lang === 'json' || lang === 'jsonc') {
    return esc(code).replace(/(&quot;(?:[^&]|&(?!quot;))*?&quot;)(\s*:)?|\b(true|false|null)\b|(-?\b\d+(?:\.\d+)?\b)/g, (m, str, colon, lit, num) =>
      str ? `<span class="${colon ? 'tk-key' : 'tk-str'}">${str}</span>${colon || ''}` : lit ? `<span class="tk-lit">${lit}</span>` : `<span class="tk-num">${num}</span>`);
  }
  if (['sh', 'fish', 'bash', 'shell', 'console'].includes(lang)) {
    return code.split('\n').map(line => {
      if (/^\s*#/.test(line)) return `<span class="tk-comment">${esc(line)}</span>`;
      const m = line.match(/^(\s*(?:\$\s+)?)([\w./-]+)(.*)$/);
      if (!m) return esc(line);
      const rest = esc(m[3]).replace(/(&quot;.*?&quot;|'[^']*')/g, '<span class="tk-str">$1</span>').replace(/(\s)(--?[\w-]+)/g, '$1<span class="tk-flag">$2</span>');
      return `${esc(m[1])}<span class="tk-cmd">${esc(m[2])}</span>${rest}`;
    }).join('\n');
  }
  return esc(code);
}

// ── Images: read intrinsic sizes so pages do not shift as they load ─────────
async function imageSize(src) {
  try {
    const bytes = await readFile(resolve(root, '.' + src));
    if (bytes.toString('ascii', 1, 4) === 'PNG') return [bytes.readUInt32BE(16), bytes.readUInt32BE(20)];
    if (bytes.toString('ascii', 8, 12) === 'WEBP') {
      const kind = bytes.toString('ascii', 12, 16);
      if (kind === 'VP8X') return [1 + bytes.readUIntLE(24, 3), 1 + bytes.readUIntLE(27, 3)];
      if (kind === 'VP8L') { const b = bytes.readUInt32LE(21); return [1 + (b & 0x3fff), 1 + ((b >> 14) & 0x3fff)]; }
      if (kind === 'VP8 ') return [bytes.readUInt16LE(26) & 0x3fff, bytes.readUInt16LE(28) & 0x3fff];
    }
  } catch {}
  return null;
}

// ── Blocks ──────────────────────────────────────────────────────────────────
export async function render(md) {
  const lines = md.replace(/\r/g, '').split('\n');
  const out = [];
  const toc = [];
  const ids = new Set();
  const uniqueId = text => { let id = slugify(text) || 'section'; let n = 2; while (ids.has(id)) id = `${slugify(text)}-${n++}`; ids.add(id); return id; };
  let i = 0;
  const isBlockStart = line => /^(#{2,4} |```|> |\s*[-*] |\s*\d+\. |\||!\[)/.test(line) || line.trim() === '';
  while (i < lines.length) {
    const line = lines[i];
    if (!line.trim()) { i++; continue; }
    let m;
    if ((m = line.match(/^(#{2,4}) (.+)$/))) {
      const level = m[1].length;
      const id = uniqueId(m[2]);
      if (level === 2) toc.push({ id, text: plain(m[2]) });
      out.push(`<h${level} id="${id}"><a class="anchor" href="#${id}" aria-hidden="true" tabindex="-1">#</a>${inline(m[2])}</h${level}>`);
      i++; continue;
    }
    if ((m = line.match(/^```(\S*)\s*(.*)$/))) {
      const lang = m[1];
      const title = m[2];
      const body = [];
      i++;
      while (i < lines.length && !lines[i].startsWith('```')) body.push(lines[i++]);
      i++;
      const code = body.join('\n');
      const label = title || { sh: 'Terminal', fish: 'Terminal', bash: 'Terminal', json: 'JSON', toml: 'TOML', text: '', jsonc: 'JSON' }[lang] || lang;
      out.push(`<div class="code-block"${lang ? ` data-lang="${esc(lang)}"` : ''}>${label ? `<div class="code-head"><span>${esc(label)}</span></div>` : ''}<button class="code-copy" type="button" aria-label="Copy code"><span class="icon" aria-hidden="true"></span></button><pre><code>${highlight(code, lang)}</code></pre></div>`);
      continue;
    }
    if ((m = line.match(/^> \[!(NOTE|TIP|WARNING)\]\s*$/))) {
      const kind = m[1].toLowerCase();
      const body = [];
      i++;
      while (i < lines.length && lines[i].startsWith('>')) body.push(lines[i++].replace(/^>\s?/, ''));
      const label = { note: 'Note', tip: 'Tip', warning: 'Warning' }[kind];
      out.push(`<aside class="callout ${kind}"><strong>${label}</strong>${(await render(body.join('\n'))).html}</aside>`);
      continue;
    }
    if (line.startsWith('> ')) {
      const body = [];
      while (i < lines.length && lines[i].startsWith('>')) body.push(lines[i++].replace(/^>\s?/, ''));
      out.push(`<blockquote>${(await render(body.join('\n'))).html}</blockquote>`);
      continue;
    }
    if ((m = line.match(/^!\[([^\]]*)\]\(([^)\s]+)\)\s*$/))) {
      const size = await imageSize(m[2]);
      const dims = size ? ` width="${size[0]}" height="${size[1]}"` : '';
      out.push(`<figure class="doc-shot"><img src="${m[2]}"${dims} alt="${esc(m[1])}" loading="lazy" decoding="async">${m[1] ? `<figcaption>${inline(m[1])}</figcaption>` : ''}</figure>`);
      i++; continue;
    }
    if (line.startsWith('|')) {
      const rows = [];
      while (i < lines.length && lines[i].startsWith('|')) rows.push(lines[i++]);
      const cells = row => row.trim().replace(/^\||\|$/g, '').split(/(?<!\\)\|/).map(cell => cell.trim().replace(/\\\|/g, '|'));
      const head = cells(rows[0]);
      const align = cells(rows[1]).map(c => c.startsWith(':') && c.endsWith(':') ? 'center' : c.endsWith(':') ? 'right' : '');
      const td = (tag, c, j) => `<${tag}${align[j] ? ` style="text-align:${align[j]}"` : ''}>${inline(c)}</${tag}>`;
      out.push(`<div class="table-wrap"><table><thead><tr>${head.map((c, j) => td('th', c, j)).join('')}</tr></thead><tbody>${rows.slice(2).map(r => `<tr>${cells(r).map((c, j) => td('td', c, j)).join('')}</tr>`).join('')}</tbody></table></div>`);
      continue;
    }
    if ((m = line.match(/^\s*([-*]|\d+\.) /))) {
      const ordered = /\d/.test(m[1]);
      const items = [];
      while (i < lines.length) {
        const item = lines[i].match(/^\s*([-*]|\d+\.) (.*)$/);
        if (item && /\d/.test(item[1]) === ordered) { items.push(item[2]); i++; continue; }
        if (lines[i].trim() && /^\s{2,}\S/.test(lines[i]) && items.length) { items[items.length - 1] += ' ' + lines[i].trim(); i++; continue; }
        break;
      }
      const tag = ordered ? 'ol' : 'ul';
      const start = ordered ? parseInt(m[1], 10) : 1;
      out.push(`<${tag}${start > 1 ? ` start="${start}"` : ''}>${items.map(item => `<li>${inline(item)}</li>`).join('')}</${tag}>`);
      continue;
    }
    const para = [];
    while (i < lines.length && lines[i].trim() && !(para.length && isBlockStart(lines[i]))) para.push(lines[i++].trim());
    out.push(`<p>${inline(para.join(' '))}</p>`);
  }
  return { html: out.join('\n'), toc };
}

export function frontmatter(text, file) {
  const m = text.match(/^---\n([\s\S]*?)\n---\n?/);
  if (!m) throw new Error(`${file}: missing frontmatter`);
  const meta = Object.fromEntries(m[1].split('\n').filter(Boolean).map(line => {
    const at = line.indexOf(':');
    return [line.slice(0, at).trim(), line.slice(at + 1).trim().replace(/^"(.*)"$/, '$1')];
  }));
  if (!meta.title || !meta.description) throw new Error(`${file}: title and description are required`);
  if (meta.description.length < 80 || meta.description.length > 180) throw new Error(`${file}: description must be 80–180 characters (${meta.description.length})`);
  return { meta, body: text.slice(m[0].length).trim() + '\n' };
}

// ── Page chrome ─────────────────────────────────────────────────────────────
export const header = (active = '') => `<header class="site-header">
      <a class="wordmark" href="/" aria-label="Diri home"><img class="app-mark" src="/assets/brand/diri-icon.svg" width="26" height="26" alt="">diri</a>
      <nav class="site-nav" aria-label="Main">${[['/#features', 'Features'], ['/guides/', 'Guides'], ['/docs/', 'Docs']].map(([href, label]) => `<a href="${href}"${label === active ? ' aria-current="page"' : ''}>${label}</a>`).join('')}</nav>
      <div class="header-actions"><a class="github-link" href="${repo}" aria-label="Diri on GitHub" title="Diri on GitHub"><span class="icon" data-icon="github" aria-hidden="true"></span><span class="star-count" aria-hidden="true"></span></a><a class="nav-download" href="${repo}/releases/latest">Download</a></div>
    </header>`;
export const footer = `<footer class="site-footer">
      <div class="footer-main">
        <div class="footer-about">
          <a class="footer-brand" href="/" aria-label="Diri home"><img class="app-mark" src="/assets/brand/diri-icon.svg" width="26" height="26" alt="">diri</a>
          <p>The best way to work with coding agents.</p>
        </div>
        <nav class="footer-columns" aria-label="Resources">
          <div><h3>Product</h3><a href="${repo}/releases/latest">Download</a><a href="/whats-new/">What's new</a><a href="${repo}/releases">Release notes</a><a href="/guides/">Guides</a><a href="/agents/">Agents</a><a href="/compare/">Compare</a></div>
          <div><h3>Project</h3><a href="${repo}">GitHub</a><a href="/docs/">Documentation</a><a href="${repo}/blob/main/ROADMAP.md">Roadmap</a></div>
          <div><h3>Legal</h3><a href="${repo}/blob/main/LICENSE">AGPL-3.0</a><a href="${repo}/blob/main/PRIVACY.md">Privacy</a><a href="${repo}/blob/main/SECURITY.md">Security</a></div>
        </nav>
      </div>
    </footer>`;


// Every generated page gets its own share card at /og/<path>.jpg (scripts/og.mjs renders them).
export const ogKey = url => url.replace(/^\/|\/$/g, '').replace(/[^a-z0-9.]+/gi, '-').replace(/\./g, '-') || 'home';
export const ogPath = url => `/og/${ogKey(url)}.jpg`;

// The install box at the end of agent, comparison and release pages.
export const installBox = (heading = 'Try diri') => `<aside class="install-box">
          <div class="install-copy"><strong>${esc(heading)}</strong><span>Free and open source. macOS 15 or newer, Linux in beta. Bring the agent CLIs and accounts you already have.</span></div>
          <div class="install-actions"><a class="button primary" href="${repo}/releases/latest"><span class="icon" data-icon="download" aria-hidden="true"></span>Download for macOS</a><div class="code-block install-brew" data-lang="sh"><button class="code-copy" type="button" aria-label="Copy code"><span class="icon" aria-hidden="true"></span></button><pre><code><span class="tk-cmd">brew</span> install <span class="tk-flag">--cask</span> cristicretu/diri/diri</code></pre></div></div>
        </aside>`;

// A long-form page outside the docs: agents, comparisons, releases.
export function article({ url, title, docTitle, description, eyebrow, eyebrowHref, lead, chips = [], html, toc = [], raw, ld, cta, after = '', heroIcon = '' }) {
  const tocHtml = toc.length > 1 ? `<nav class="guide-toc" aria-label="On this page"><span>On this page</span>${toc.map(t => `<a href="#${t.id}">${esc(t.text)}</a>`).join('')}</nav>` : '<span></span>';
  return `<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <meta name="theme-color" content="#191724">
  <title>${esc(docTitle || `${title} · Diri`)}</title>
  <meta name="description" content="${esc(description)}">
  <meta name="robots" content="index, follow, max-image-preview:large">
  <link rel="canonical" href="${site}${url}">${raw ? `\n  <link rel="alternate" type="text/markdown" href="${raw}">` : ''}
  <meta property="og:type" content="article">
  <meta property="og:site_name" content="Diri">
  <meta property="og:title" content="${esc(title)}">
  <meta property="og:description" content="${esc(description)}">
  <meta property="og:url" content="${site}${url}">
  <meta property="og:image" content="${site}${ogPath(url)}">
  <meta property="og:image:width" content="1200">
  <meta property="og:image:height" content="630">
  <meta property="og:image:alt" content="${esc(title)}">
  <meta name="twitter:card" content="summary_large_image">
  <link rel="icon" href="/favicon.svg" type="image/svg+xml">
  <link rel="stylesheet" href="/style.css">
  <link rel="stylesheet" href="/guides.css">
  <link rel="stylesheet" href="/docs.css">
  <script src="/docs.js" defer></script>
  <script type="application/ld+json">${JSON.stringify(ld)}</script>
  <script type="module" src="/stats.js"></script>
</head>
<body class="guide-page article-page">
  <a class="skip-link" href="#main">Skip to content</a>
  <div class="site-wrap">
    ${header()}
    <main id="main">
      <header class="guide-heading">${heroIcon}${eyebrowHref ? `<a class="eyebrow" href="${eyebrowHref}">${esc(eyebrow)}</a>` : `<span class="eyebrow">${esc(eyebrow)}</span>`}<h1>${esc(title)}</h1><p>${inline(lead || description)}</p>${chips.length ? `
        <div class="guide-meta">${chips.map(c => `<span>${esc(c)}</span>`).join('')}</div>` : ''}</header>
      <div class="guide-layout">${tocHtml}
        <article class="docs-content article-content">
${html}
        ${cta === false ? '' : installBox(cta)}
        ${after}
        </article>
      </div>
    </main>
    ${footer}
  </div>
</body>
</html>
`;
}
