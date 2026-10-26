// Pages outside the docs, generated from data the repository already has:
// one page per agent (from the Engine's manifests), comparisons (compare-src/*.md),
// and one page per release (whats-new-src/releases.json).
import { readFile, readdir } from 'node:fs/promises';
import { resolve } from 'node:path';
import { root, site, repo, today, esc, render, frontmatter, article, ogPath, footer } from './render.mjs';

const manifests = resolve(root, '../diri/crates/diri-engine/manifests');
const LOGOS = { 'claude-code': 'claude', codex: 'codex', copilot: 'copilot', cursor: 'cursor', gemini: 'gemini', kimi: 'kimi', opencode: 'opencode', pi: 'pi' };
const ORCHESTRATORS = 'Claude Code, Codex and Cursor';

const publisher = { '@type': 'Organization', name: 'Diri', url: `${site}/` };

// ── Agents ──────────────────────────────────────────────────────────────────
export async function loadAgents() {
  const agents = [];
  for (const file of (await readdir(manifests)).filter(f => f.endsWith('.json')).sort()) {
    const manifest = JSON.parse(await readFile(resolve(manifests, file), 'utf8'));
    const a = manifest.agent;
    if (!a?.binary || ['shell', 'generic'].includes(manifest.id)) continue;
    const injection = a.injection || {};
    const conversation = a.conversation || {};
    const resume = conversation.resume?.exactArgs ? 'exact'
      : (conversation.freshArgs || []).some(arg => arg.includes('{sessionDir}')) ? 'session'
      : a.resume ? 'latest' : 'none';
    agents.push({
      id: manifest.id,
      name: a.displayName,
      short: a.shortLabel,
      binary: a.binary,
      order: a.catalogOrder ?? 1000,
      hooks: a.statusAuthority === 'hooks',
      turnNotify: !!injection.codexNotify,
      lifecycleHooks: !!(injection.claudeHooks || injection.cursorHooks),
      resume,
      fork: !!conversation.fork,
      approve: !!a.approve,
      mcp: !!(injection.claudeMCP || injection.codexMCP || injection.cursorMCP),
      setup: a.setup || {},
      logo: LOGOS[manifest.id],
    });
  }
  return agents.sort((x, y) => x.order - y.order || x.name.localeCompare(y.name));
}

const RESUME = {
  exact: 'Yes, the exact conversation. diri records its id at launch and reopens that one.',
  session: 'Yes. diri gives each session its own storage folder, so continuing picks the right conversation.',
  latest: "Yes, through the agent's own continue flag. It reopens the most recent conversation in that folder, which can be a different one if you also ran the agent there outside diri.",
  none: 'Not automatically. The terminal and its history survive restarts, but a session that exits starts a fresh conversation.',
};

function agentMarkdown(agent, agents) {
  const n = agent.name;
  const statusRow = agent.hooks ? 'From its own lifecycle hooks, so working, waiting and done are exact.'
    : agent.turnNotify ? 'Read from the screen, plus a callback when each turn finishes.'
    : agent.lifecycleHooks ? 'Read from the screen, plus a hook when it stops.'
    : 'Read from the screen with rules written for its interface.';
  const rows = [
    ['Live status', statusRow],
    ['Needs-you alerts', `A notification and an amber mark when ${n} asks a question or wants permission.`],
    ['Own worktree', 'Optional per session, so parallel runs never edit the same checkout.'],
    ['Review', 'Diffs, staging, commits and pull request checks beside the session.'],
    ['Survives restarts', 'Yes. Each session is owned by its own process, so quitting diri never stops it.'],
    ['Resume after exit', RESUME[agent.resume].split('.')[0] + '.'],
    ['Fork a conversation', agent.fork ? 'Yes. Branch a conversation to try another approach.' : 'Not supported.'],
    ['Approve from a notification', agent.approve ? 'Yes. Permission prompts get an Approve button on the notification.' : 'No. Answer permission prompts in the terminal.'],
    ['Starts other agents', agent.mcp ? "Yes. diri's MCP server is added at launch." : `No built-in connection. ${ORCHESTRATORS} can start ${n} sessions for you.`],
  ];
  const install = agent.setup.installCommand
    ? `Install ${n} with its official installer${agent.setup.installRequirement ? ` (needs ${agent.setup.installRequirement})` : ''}. diri can also run this for you: **Settings → Agents → Install** shows the command and runs it only after you confirm.\n\n\`\`\`sh\n${agent.setup.installCommand}\n\`\`\``
    : `${agent.setup.installHint || `Install ${n}.`}${agent.setup.url ? ` See the [${n} install guide](${agent.setup.url}).` : ''}`;
  const others = agents.filter(o => o.id !== agent.id).slice(0, 8).map(o => `[${o.name}](/agents/${o.id}/)`).join(' · ');
  const spawnPrompt = agent.mcp
    ? `Split this into three parts and start a ${n} agent for each, in its own worktree. Wait for them, review each diff, and merge the ones that pass the tests.`
    : `Start two ${n} agents in their own worktrees: one fixes the failing tests, the other updates the docs. Tell me when both are done.`;
  return `diri runs \`${agent.binary}\` exactly as you would in a terminal, with your own install and sign-in. What it adds is everything one terminal window can't give you: several ${n} sessions at once, each in its own git worktree, a sidebar that shows which ones are working and which need you, and one place to review what they changed.

## What diri adds for ${n}

| | |
| --- | --- |
${rows.map(([k, v]) => `| ${k} | ${v} |`).join('\n')}

## Set up

1. ${install.split('\n')[0]}
${install.includes('```') ? '\n' + install.split('\n').slice(2).join('\n') + '\n' : ''}
2. ${agent.setup.signInHint || `Start \`${agent.binary}\` once and sign in.`}
3. Install diri with \`brew install --cask diri\`, or [download it](${repo}/releases/latest).
4. Open **New Agent** in the sidebar and choose **${n}**. If you installed ${n} while diri was open, click **Refresh** in **Settings → Agents** first.

The [quickstart](/docs/quickstart/) covers the rest: picking a folder, giving a task and answering questions.

## Run several at once

Start more sessions from **New Agent** with a fresh worktree each, or let an agent do it. ${agent.mcp ? `${n} gets diri's [MCP server](/docs/mcp/) automatically, so you can ask it in plain words:` : `${ORCHESTRATORS} get diri's [MCP server](/docs/mcp/) automatically and can start ${n} sessions (kind \`${agent.short}\`). Ask one of them:`}

> ${spawnPrompt}

Each session shows up in the sidebar under the agent that started it. [Worktrees and review](/docs/worktrees/) explains how to bring the work back.

## Questions

### Does diri need its own ${n} account?
No. diri starts the \`${agent.binary}\` you installed, which uses its own sign-in. diri has no account of its own and never sees your password.

### Can I pick a ${n} session up again later?
${RESUME[agent.resume]} Quitting diri never ends a session in the first place.

### Can ${n} run on a server?
Yes. Install and sign in to ${n} on any machine you reach with \`ssh\`, then pick it as the machine in **New Agent**. diri needs no tmux, sudo or service there. See [Remote hosts](/docs/remote-hosts/).

### What does diri cost?
Nothing. diri is free and open source under AGPL-3.0. You pay for ${n} as you do today.

## Other agents

${others} · [All agents](/agents/)
`;
}

const agentLogo = (agent, size = 'lg') => agent.logo
  ? `<span class="agent-tile ${size}"><span class="icon" style="--icon:url('/assets/brand/${agent.logo}.svg')" aria-hidden="true"></span></span>`
  : `<span class="agent-tile ${size} letter" aria-hidden="true">${esc(agent.name[0])}</span>`;

async function buildAgents(outputs, cards, entries) {
  const agents = await loadAgents();
  for (const agent of agents) {
    const url = `/agents/${agent.id}/`;
    const md = agentMarkdown(agent, agents);
    const { html, toc } = await render(md);
    const title = `Run ${agent.name} in parallel`;
    const description = `Run several ${agent.name} sessions side by side in diri, each in its own git worktree, with live status, alerts when it needs you, and review in one place.`;
    outputs.set(`agents/${agent.id}/index.html`, article({
      url, title, docTitle: `${agent.name} in parallel, with worktrees and live status · Diri`, description, eyebrow: 'Agents', eyebrowHref: '/agents/',
      lead: `Run as many ${agent.name} sessions as you like, each on its own branch, and step in only when one needs you.`,
      chips: ['Free and open source', agent.hooks ? 'Exact status' : 'Live status', ...(agent.mcp ? ['Starts other agents'] : [])],
      html, toc, raw: `/agents/${agent.id}.md`, cta: `Run ${agent.name} in diri`, heroIcon: agentLogo(agent),
      ld: { '@context': 'https://schema.org', '@type': 'TechArticle', headline: title, description, url: site + url, inLanguage: 'en', dateModified: today, publisher, about: { '@type': 'SoftwareApplication', name: agent.name } },
    }));
    outputs.set(`agents/${agent.id}.md`, `# ${title}\n\n> ${description}\n\n${md}`);
    cards.push({ url, eyebrow: 'Agents', title: `Run ${agent.name} in parallel`, logo: agent.logo, letter: agent.logo ? undefined : agent.name[0] });
    entries.push({ group: 'Agents', url, md: `/agents/${agent.id}.md`, title, description });
  }

  // Index: every agent with the facts that differ between them.
  const yes = v => v ? 'Yes' : '—';
  const resumeLabel = { exact: 'Exact', session: 'Per session', latest: 'Latest', none: '—' };
  const md = `diri runs ${agents.length} terminal coding agents out of the box, plus any other command you point it at. Every one gets live status, notifications, its own worktree and the review panel. The table shows where they differ. [Supported agents](/docs/agents/) in the docs explains each column.

| Agent | Status | Resume | Fork | Starts other agents |
| --- | --- | --- | --- | --- |
${agents.map(a => `| [${a.name}](/agents/${a.id}/) | ${a.hooks ? 'Hooks' : 'Screen'} | ${resumeLabel[a.resume]} | ${yes(a.fork)} | ${a.mcp ? 'Yes' : '—'} |`).join('\n')}

Missing yours? Any terminal agent runs in diri as a plain session, and a short JSON manifest teaches diri its status and resume rules. See [Add your own agent](/docs/agents/#add-your-own-agent).
`;
  const { html } = await render(md);
  const grid = `<ul class="agent-grid">${agents.map(a => `<li><a href="/agents/${a.id}/">${agentLogo(a, 'sm')}<span>${esc(a.name)}</span></a></li>`).join('')}</ul>`;
  const description = `Every coding agent diri runs out of the box, from Claude Code and Codex to Gemini, Copilot and OpenCode, and what diri adds to each.`;
  outputs.set('agents/index.html', article({
    url: '/agents/', title: 'Every agent, side by side', docTitle: 'Supported coding agents · Diri', description, eyebrow: 'Agents',
    lead: `Claude Code, Codex, Cursor, Gemini and ${agents.length - 4} more, with the CLIs and accounts you already have.`,
    html: grid + '\n' + html, cta: 'Run them all in diri',
    ld: { '@context': 'https://schema.org', '@type': 'CollectionPage', headline: 'Supported coding agents', description, url: `${site}/agents/`, inLanguage: 'en', publisher },
  }));
  cards.push({ url: '/agents/', eyebrow: 'Agents', title: `${agents.length} coding agents, side by side` });
  return ['/agents/', ...agents.map(a => `/agents/${a.id}/`)];
}

// ── Comparisons ─────────────────────────────────────────────────────────────
async function buildCompare(outputs, cards, entries) {
  const dir = resolve(root, 'compare-src');
  const files = (await readdir(dir).catch(() => [])).filter(f => f.endsWith('.md')).sort();
  const list = [];
  for (const file of files) {
    const slug = file.replace(/\.md$/, '');
    const { meta, body } = frontmatter(await readFile(resolve(dir, file), 'utf8'), `compare-src/${file}`);
    const url = `/compare/${slug}/`;
    const { html, toc } = await render(body);
    outputs.set(`compare/${slug}/index.html`, article({
      url, title: meta.title, docTitle: `${meta.title}: ${meta.competitor} alternative · Diri`, description: meta.description, eyebrow: 'Compare', eyebrowHref: '/compare/',
      lead: meta.lead || meta.description, chips: [`Checked ${meta.checked}`, 'Sources linked'], html, toc, raw: `/compare/${slug}.md`, cta: 'Try diri side by side',
      ld: { '@context': 'https://schema.org', '@type': 'TechArticle', headline: meta.title, description: meta.description, url: site + url, inLanguage: 'en', dateModified: meta.checked, publisher },
    }));
    outputs.set(`compare/${slug}.md`, `# ${meta.title}\n\n> ${meta.description}\n\n${body}`);
    cards.push({ url, eyebrow: 'Compare', title: meta.title });
    entries.push({ group: 'Compare', url, md: `/compare/${slug}.md`, title: meta.title, description: meta.description });
    list.push({ url, meta });
  }
  if (!list.length) return [];
  const md = `Honest notes on how diri differs from other tools for running coding agents, and when the other tool is the better fit. Each page lists the sources it was checked against and the date.\n\n${list.map(({ url, meta }) => `### [${meta.title}](${url})\n\n${meta.description}`).join('\n\n')}\n`;
  const { html } = await render(md);
  const description = 'How diri compares with other apps for running coding agents in parallel: what is different, where the alternative fits better, with sources.';
  outputs.set('compare/index.html', article({
    url: '/compare/', title: 'diri compared', docTitle: 'diri compared with other agent tools · Diri', description, eyebrow: 'Compare', html, cta: 'Try diri',
    ld: { '@context': 'https://schema.org', '@type': 'CollectionPage', headline: 'diri compared', description, url: `${site}/compare/`, inLanguage: 'en', publisher },
  }));
  cards.push({ url: '/compare/', eyebrow: 'Compare', title: 'diri compared' });
  return ['/compare/', ...list.map(({ url }) => url)];
}

// ── Releases ────────────────────────────────────────────────────────────────
const clip = item => {
  if (!item.clip) return '';
  const missing = ['width', 'height', 'alt'].filter(key => !item[key]);
  if (missing.length) throw new Error(`whats-new-src/releases.json: clip ${item.clip} needs ${missing.join(', ')}`);
  return `<figure class="release-clip"><img src="${item.clip}" width="${item.width}" height="${item.height}" alt="${esc(item.alt)}" loading="lazy" decoding="async"></figure>`;
};
const releaseItems = release => release.items.map(item => `<article class="release-item">${clip(item)}<h3>${esc(item.title)}</h3><p>${esc(item.text)}</p></article>`).join('');

async function buildReleases(outputs, cards, entries) {
  const releases = JSON.parse(await readFile(resolve(root, 'whats-new-src/releases.json'), 'utf8'));
  const urls = [];
  for (const [i, release] of releases.entries()) {
    const url = `/whats-new/${release.version}/`;
    const title = `diri ${release.version}: ${release.headline}`;
    const newer = releases[i - 1];
    const older = releases[i + 1];
    const pager = `<nav class="docs-pager" aria-label="Other releases">${older ? `<a class="prev" href="/whats-new/${older.version}/"><span>Older</span>diri ${older.version}</a>` : '<span></span>'}${newer ? `<a class="next" href="/whats-new/${newer.version}/"><span>Newer</span>diri ${newer.version}</a>` : `<a class="next" href="/whats-new/"><span>All releases</span>What's new</a>`}</nav>`;
    const html = `<div class="whats-new release-page"><section class="release">${releaseItems(release)}</section></div>
<p>Every fix and smaller change is in the <a href="${repo}/releases/tag/v${release.version}">full release notes</a>. diri updates itself; this release shows up as a restart prompt when it is ready.</p>`;
    outputs.set(`whats-new/${release.version}/index.html`, article({
      url, title, docTitle: `${title} · Diri`, description: release.summary, eyebrow: `What's new · ${release.date}`, eyebrowHref: '/whats-new/',
      lead: release.summary, html, cta: `Get diri ${release.version}`, after: pager,
      ld: { '@context': 'https://schema.org', '@type': 'TechArticle', headline: title, description: release.summary, url: site + url, inLanguage: 'en', datePublished: new Date(`${release.date} 00:00 UTC`).toISOString().slice(0, 10), publisher },
    }));
    cards.push({ url, eyebrow: `Release · ${release.date}`, title: `diri ${release.version}`, subtitle: release.headline });
    entries.push({ group: 'Releases', url, title, description: release.summary });
    urls.push(url);
  }
  // The What's new index keeps its hand-written head; its releases come from the same data.
  const indexPath = resolve(root, 'whats-new/index.html');
  const index = await readFile(indexPath, 'utf8');
  const sections = releases.map(r => `<section class="release" id="v${r.version}"><header class="release-heading"><h2><a href="/whats-new/${r.version}/">diri ${r.version}</a></h2><span>${esc(r.date)}</span></header>${releaseItems(r)}</section>`).join('\n      ');
  outputs.set('whats-new/index.html', index
    .replace(/(<\/header>\n      )<section class="release"[\s\S]*<\/section>(\n    <\/main>)/, `$1${sections}$2`)
    .replace(/(<meta property="og:image" content=")[^"]+(")/, `$1${site}${ogPath('/whats-new/')}$2`));
  cards.push({ url: '/whats-new/', eyebrow: "What's new", title: `diri ${releases[0].version}`, subtitle: releases[0].headline });
  return ['/whats-new/', ...urls];
}

// Hand-written pages keep their HTML; only their share card is generated.
async function retagGuides(outputs, cards) {
  const pages = ['guides/index.html', ...(await readdir(resolve(root, 'guides'), { withFileTypes: true })).filter(d => d.isDirectory()).map(d => `guides/${d.name}/index.html`)];
  for (const page of pages.sort()) {
    const html = await readFile(resolve(root, page), 'utf8');
    const url = '/' + page.replace(/index\.html$/, '');
    const title = html.match(/<h1>(.*?)<\/h1>/)[1].replace(/<[^>]+>/g, '');
    outputs.set(page, html.replace(/(<meta property="og:image" content=")[^"]+(")/, `$1${site}${ogPath(url)}$2`));
    cards.push({ url, eyebrow: page === 'guides/index.html' ? 'Guides' : 'Guide', title });
  }
}

export async function buildPages() {
  const outputs = new Map();
  const cards = [];
  const entries = [];
  const urls = [
    ...await buildAgents(outputs, cards, entries),
    ...await buildCompare(outputs, cards, entries),
    ...await buildReleases(outputs, cards, entries),
  ];
  await retagGuides(outputs, cards);
  await syncFooters(outputs);
  return { outputs, cards, entries, urls };
}

// Hand-written pages share the generated footer, so a license or link change lands everywhere.
async function syncFooters(outputs) {
  outputs.set('index.html', await readFile(resolve(root, 'index.html'), 'utf8'));
  for (const [page, html] of outputs) {
    if (!page.endsWith('.html') || !html.includes('<footer class="site-footer">')) continue;
    outputs.set(page, html.replace(/<footer class="site-footer">[\s\S]*?<\/footer>/, () => footer));
  }
}
