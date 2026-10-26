// Docs: ⌘K search, copy buttons, the "on this page" highlight, and the phone menu.
const $ = (selector, root = document) => root.querySelector(selector);
const $$ = (selector, root = document) => [...root.querySelectorAll(selector)];

// Homebrew casks are macOS only, so install boxes show the brew line on Macs and leave it hidden elsewhere.
// iPads can report a Mac platform in desktop mode, so touch screens are excluded.
const onMac = /Mac/.test(navigator.userAgentData?.platform ?? navigator.platform ?? '') && navigator.maxTouchPoints < 2;
document.documentElement.classList.toggle('is-mac', onMac);

const masthead = $('.site-header');
if (masthead) {
  const sync = () => masthead.classList.toggle('scrolled', scrollY > 8);
  addEventListener('scroll', sync, { passive: true });
  sync();
}

async function copy(text, button, done) {
  try { await navigator.clipboard.writeText(text); done(true); }
  catch { done(false); }
  void button;
}

for (const button of $$('.code-copy')) {
  button.addEventListener('click', () => copy($('code', button.parentElement).textContent, button, ok => {
    button.classList.toggle('copied', ok);
    button.setAttribute('aria-label', ok ? 'Copied' : 'Select to copy');
    setTimeout(() => { button.classList.remove('copied'); button.setAttribute('aria-label', 'Copy code'); }, 1400);
  }));
}

const copyPage = $('[data-copy-page]');
if (copyPage) {
  const label = $('span:last-child', copyPage);
  copyPage.addEventListener('click', async () => {
    try {
      const response = await fetch($('.docs-main').dataset.raw);
      if (!response.ok) throw new Error(String(response.status));
      await copy(await response.text(), copyPage, ok => { label.textContent = ok ? 'Copied' : 'Could not copy'; });
    } catch { label.textContent = 'Could not copy'; }
    setTimeout(() => { label.textContent = 'Copy as Markdown'; }, 1600);
  });
}

// Phone: the sidebar folds under a breadcrumb button.
const menu = $('.docs-menu');
menu?.addEventListener('click', () => menu.setAttribute('aria-expanded', String(menu.getAttribute('aria-expanded') !== 'true')));

// "On this page": highlight the last heading that scrolled past the masthead.
const tocLinks = $$('.docs-toc a');
if (tocLinks.length) {
  const targets = tocLinks.map(link => document.getElementById(decodeURIComponent(link.hash.slice(1)))).filter(Boolean);
  let frame = 0;
  const spy = () => {
    frame = 0;
    let current = targets[0];
    for (const target of targets) if (target.getBoundingClientRect().top < 120) current = target;
    if (innerHeight + scrollY >= document.documentElement.scrollHeight - 4) current = targets[targets.length - 1];
    for (const link of tocLinks) link.classList.toggle('active', link.hash === `#${current.id}`);
  };
  addEventListener('scroll', () => { frame ||= requestAnimationFrame(spy); }, { passive: true });
  spy();
}

// ⌘K palette.
const dialog = $('.docs-search-dialog');
if (dialog) {
  const input = $('input', dialog);
  const list = $('.docs-search-results', dialog);
  let index = null;
  let rank = null;
  let results = [];
  let selected = 0;
  let opener = null;

  const escapeHtml = text => text.replace(/[&<>"]/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));
  const mark = (text, words) => {
    let html = escapeHtml(text);
    for (const word of words) if (word.length > 1) html = html.replace(new RegExp(`(${word.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')})`, 'gi'), '<mark>$1</mark>');
    return html;
  };

  async function load() {
    if (index) return;
    const [data, module] = await Promise.all([fetch('/docs/search.json').then(r => r.json()), import('/docs-search.js')]);
    index = data;
    rank = module.rank;
  }

  function draw() {
    const query = input.value.trim();
    const words = query.toLowerCase().split(/\s+/).filter(Boolean);
    if (!query) {
      results = (index || []).filter(entry => !entry.p);
    } else {
      results = rank ? rank(index, query, 14) : [];
    }
    selected = Math.min(selected, Math.max(results.length - 1, 0));
    if (!results.length) {
      list.innerHTML = `<li class="empty">${index ? `Nothing matches “${escapeHtml(query)}”.` : 'Loading…'}</li>`;
      return;
    }
    list.innerHTML = results.map((entry, i) => `<li role="option" id="docs-result-${i}" aria-selected="${i === selected}"><a href="${entry.u}" tabindex="-1"><span class="icon" data-icon="${entry.p ? 'return' : 'file'}" aria-hidden="true"></span><span><span class="r-title">${mark(entry.t, words)}</span><span class="r-path">${escapeHtml(entry.p ? `${entry.p}` : entry.g)}</span></span><span class="r-text">${mark(entry.x || '', words)}</span></a></li>`).join('');
    input.setAttribute('aria-activedescendant', `docs-result-${selected}`);
  }

  function move(to) {
    if (!results.length) return;
    selected = (to + results.length) % results.length;
    $$('li', list).forEach((li, i) => li.setAttribute('aria-selected', String(i === selected)));
    input.setAttribute('aria-activedescendant', `docs-result-${selected}`);
    $(`#docs-result-${selected}`)?.scrollIntoView({ block: 'nearest' });
  }

  async function open() {
    if (!dialog.hidden) return;
    opener = document.activeElement;
    dialog.hidden = false;
    document.body.classList.add('searching');
    input.value = '';
    selected = 0;
    draw();
    input.focus();
    try { await load(); draw(); } catch { list.innerHTML = '<li class="empty">Search is unavailable offline.</li>'; }
  }

  function close() {
    if (dialog.hidden) return;
    dialog.hidden = true;
    document.body.classList.remove('searching');
    opener?.focus?.();
  }

  $$('[data-open-search]').forEach(button => button.addEventListener('click', open));
  $$('[data-close-search]', dialog).forEach(el => el.addEventListener('click', close));
  input.addEventListener('input', () => { selected = 0; draw(); });
  list.addEventListener('mousemove', event => {
    const li = event.target.closest('li[role="option"]');
    if (li) { const i = $$('li', list).indexOf(li); if (i !== selected) move(i); }
  });
  list.addEventListener('click', event => { if (event.target.closest('a')) close(); });
  input.addEventListener('keydown', event => {
    if (event.key === 'ArrowDown' || (event.ctrlKey && event.key === 'n')) { event.preventDefault(); move(selected + 1); }
    else if (event.key === 'ArrowUp' || (event.ctrlKey && event.key === 'p')) { event.preventDefault(); move(selected - 1); }
    else if (event.key === 'Enter' && results[selected]) {
      event.preventDefault();
      const url = results[selected].u;
      close();
      location.href = url;
    }
  });
  addEventListener('keydown', event => {
    if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === 'k') { event.preventDefault(); dialog.hidden ? open() : close(); }
    else if (event.key === '/' && dialog.hidden && !/^(input|textarea|select)$/i.test(document.activeElement?.tagName || '')) { event.preventDefault(); open(); }
    else if (event.key === 'Escape') close();
  });
}
