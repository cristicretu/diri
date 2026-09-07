const $ = (selector, root = document) => root.querySelector(selector);
const $$ = (selector, root = document) => [...root.querySelectorAll(selector)];
const icon = name => `<span class="icon" data-icon="${name}" aria-hidden="true"></span>`;
function paintIcons(root = document) {
  $$('[data-icon], [data-agent]', root).forEach(el => {
    const folder = el.dataset.agent ? 'brand' : 'icons';
    el.style.setProperty('--icon', `url("assets/${folder}/${el.dataset.agent || el.dataset.icon}.svg")`);
    el.setAttribute('aria-hidden', 'true');
  });
}
const chats = {
  website: { title: 'A little more Diri', agent: 'claude', name: 'Claude', status: 'Ready when you are', prompt: 'A quieter home for Diri. Less noise, more room for the work.', body: `<div class="work-line">${icon('check')}<span class="work-verb">Read</span><code>the little details that make Diri, Diri</code></div><div class="work-line">${icon('check')}<span class="work-verb">Updated</span><code>website/index.html</code></div><div class="work-line">${icon('check')}<span class="work-verb">Refined</span><code>spacing, color, and the way things move</code></div><div class="terminal-result"><strong>A little more breathing room.</strong><p>The first draft is ready. Everything you need,<br>with a little less getting in the way.</p><button class="result-link" data-open="links">Take a look ${icon('external-link')}</button></div>` },
  notes: { title: 'Make sense of the notes', agent: 'codex', name: 'Codex', status: 'A quick question for you', prompt: 'Turn these scattered notes into a plan for the week.', body: `<div class="work-line">${icon('check')}<span class="work-verb">Read</span><code>monday-notes.md · ideas.md · next-up.md</code></div><div class="terminal-result"><strong>Found the thread.</strong><p>Three priorities, a few loose ends, and one good idea.<br>How much detail would you like?</p><div class="question-options"><button data-answer="short">Just the essentials</button><button data-answer="full">A little more detail</button></div></div>` },
  details: { title: 'Sweat the small stuff', agent: 'cursor', name: 'Cursor', status: 'Working on the details', prompt: 'Make the small interactions feel as good as they look.', body: `<div class="work-line">${icon('check')}<span class="work-verb">Checked</span><code>keyboard navigation and focus</code></div><div class="work-line">${icon('check')}<span class="work-verb">Refined</span><code>the command menu transition</code></div><div class="terminal-result"><strong>The details add up.</strong><p>A softer entrance. A clear way back.<br>Everything right where you expect it.</p><button class="result-link" data-open="command">Try the command menu ${icon('chevron-right')}</button></div>` },
  weekend: { title: 'The weekend idea', agent: 'gemini', name: 'Gemini', status: 'Saved for a little later', prompt: 'A tiny app for collecting places I want to visit.', body: `<div class="terminal-result"><strong>Start small. Go somewhere.</strong><p>A place, a note, and a pin on a map.<br>No itinerary required.</p><p>Your idea will be right here when you come back.</p></div>` }
};
let currentChat = 'website';
let overlay = null;
let previousFocus;
let page = 'commands';
let selected = 0;
let matches = [];
let demoTheme = 'dark';
let originalDemoTheme = null;
function announce(text) { $('#announcement').textContent = text; }
function selectChat(id) {
  const chat = chats[id];
  if (!chat) return;
  currentChat = id;
  $$('.chat-row').forEach(row => { row.classList.toggle('selected', row.dataset.chat === id); row.setAttribute('aria-pressed', String(row.dataset.chat === id)); });
  $('#session-name').textContent = chat.title;
  $('#agent-name').textContent = chat.name;
  const logo = $('.current-agent .agent-logo');
  logo.dataset.agent = chat.agent;
  logo.className = `agent-logo ${chat.agent}`;
  $('#terminal-status').textContent = chat.status;
  $('#terminal-content').innerHTML = `<div class="terminal-scene"><div class="terminal-meta"><span class="agent-logo ${chat.agent}" data-agent="${chat.agent}"></span><strong>${chat.name}</strong><span>~/sunday-studio</span></div><div class="prompt-line"><span>›</span><span>${chat.prompt}</span></div>${chat.body}</div>`;
  paintIcons();
}
function setTab(view) {
  $$('.demo-tab').forEach(tab => { const active = tab.dataset.view === view; tab.classList.toggle('active', active); tab.setAttribute('aria-selected', String(active)); tab.tabIndex = active ? 0 : -1; });
  $('#demo-window').setAttribute('aria-labelledby', `tab-${view}`);
}
function restoreTheme() {
  if (originalDemoTheme !== null) { applyDemoTheme(originalDemoTheme); originalDemoTheme = null; }
}
function closeOverlay(restoreFocus = true) {
  restoreTheme();
  $$('.floating-panel').forEach(el => { el.hidden = true; });
  $('#demo-shade').hidden = true;
  overlay = null;
  $('.app-sidebar').inert = false;
  $('.app-main').inert = false;
  setTab('workspace');
  if (restoreFocus && previousFocus?.isConnected) previousFocus.focus({ preventScroll: true });
}
function openOverlay(kind) {
  if (!overlay) previousFocus = document.activeElement;
  closeOverlay(false);
  overlay = kind;
  $('.app-sidebar').inert = true;
  $('.app-main').inert = true;
  $('#demo-shade').hidden = false;
  const panel = $(kind === 'command' ? '#palette' : `#${kind}-panel`);
  panel.hidden = false;
  setTab(kind === 'notifications' ? 'workspace' : kind);
  if (kind === 'command') { goPage('commands'); $('#palette-input').focus({ preventScroll: true }); }
  else $('button, a', panel)?.focus({ preventScroll: true });
}
function applyDemoTheme(value) { demoTheme = value; $('#demo-window').classList.toggle('theme-light', value === 'light'); }
const chatItem = (id) => ({ label: chats[id].title, agent: chats[id].agent, action: () => { selectChat(id); closeOverlay(); } });
function itemsForPage() {
  if (page === 'chats') return Object.keys(chats).map(chatItem);
  if (page === 'projects') return [{ label: 'Sunday studio', icon: 'folder', action: () => { selectChat('website'); closeOverlay(); } }, { label: 'Little experiments', icon: 'folder', action: () => { selectChat('weekend'); closeOverlay(); } }];
  if (page === 'settings') return [{ label: 'Color theme', icon: 'moon', hint: 'Choose your mood', action: () => goPage('themes') }];
  if (page === 'themes') return [{ label: 'Diri dark', theme: 'dark', color: '#393540' }, { label: 'Diri light', theme: 'light', color: '#e9e7df' }].map(item => ({ ...item, action: () => { applyDemoTheme(item.theme); originalDemoTheme = null; closeOverlay(); announce(`${item.label} selected for the preview`); } }));
  return [chatItem('website'), chatItem('notes'), { label: 'Search chats', icon: 'search', hint: '⇧⌘H', action: () => goPage('chats') }, { label: 'Open project', icon: 'folder', hint: '⌘P', action: () => goPage('projects') }, { label: 'Settings', icon: 'settings', hint: 'Appearance', action: () => goPage('settings') }];
}
function goPage(next) {
  restoreTheme();
  page = next;
  if (page === 'themes') originalDemoTheme = demoTheme;
  selected = 0;
  $('#palette-input').value = '';
  $('#palette-input').placeholder = { commands: 'Search chats or run a command…', chats: 'Search chats…', projects: 'Open a project…', settings: 'Settings', themes: 'Choose a theme…' }[page];
  $('#palette-back').hidden = page === 'commands';
  $('#palette-search-icon').hidden = page !== 'commands';
  renderPalette();
  $('#palette-input').focus({ preventScroll: true });
}
function renderPalette() {
  const query = $('#palette-input').value.trim().toLowerCase();
  matches = itemsForPage().filter(item => item.label.toLowerCase().includes(query));
  selected = Math.max(0, Math.min(selected, matches.length - 1));
  const list = $('#palette-list');
  list.replaceChildren();
  matches.forEach((item, i) => {
    const row = document.createElement('div');
    row.className = 'palette-option';
    row.id = `command-${i}`;
    row.setAttribute('role', 'option');
    row.setAttribute('aria-selected', String(i === selected));
    row.innerHTML = `${item.theme ? `<span class="theme-swatch" style="background:${item.color}"></span>` : item.agent ? `<span class="agent-logo ${item.agent}" data-agent="${item.agent}"></span>` : icon(item.icon)}<span>${item.label}</span>${item.hint ? `<small>${item.hint}</small>` : ''}${item.theme === demoTheme ? icon('check') : ''}`;
    row.addEventListener('mousedown', event => event.preventDefault());
    row.addEventListener('click', item.action);
    row.addEventListener('pointermove', () => highlight(i));
    list.append(row);
  });
  if (!matches.length) { const empty = document.createElement('p'); empty.className = 'palette-empty'; empty.textContent = 'Nothing here yet. Try another search.'; list.append(empty); }
  paintIcons(list);
  highlight(selected);
}
function highlight(index) {
  selected = index;
  $$('.palette-option').forEach((row, i) => row.setAttribute('aria-selected', String(i === index)));
  const item = matches[index];
  if (item) $('#palette-input').setAttribute('aria-activedescendant', `command-${index}`);
  else $('#palette-input').removeAttribute('aria-activedescendant');
  if (item?.theme) applyDemoTheme(item.theme);
  const row = $(`#command-${index}`);
  const list = $('#palette-list');
  if (row && row.offsetTop < list.scrollTop) list.scrollTop = row.offsetTop;
  else if (row && row.offsetTop + row.offsetHeight > list.scrollTop + list.clientHeight) list.scrollTop = row.offsetTop + row.offsetHeight - list.clientHeight;
}
$('#palette-input').addEventListener('input', () => { selected = 0; renderPalette(); });
$('#palette-input').addEventListener('keydown', event => {
  if (['ArrowDown', 'ArrowUp'].includes(event.key)) { event.preventDefault(); if (matches.length) highlight((selected + (event.key === 'ArrowDown' ? 1 : -1) + matches.length) % matches.length); }
  if (event.key === 'Enter') { event.preventDefault(); matches[selected]?.action(); }
  if (event.key === 'Backspace' && !event.target.value && page !== 'commands') { event.preventDefault(); goPage(page === 'themes' ? 'settings' : 'commands'); }
});
$('#palette-back').addEventListener('click', () => goPage(page === 'themes' ? 'settings' : 'commands'));
$('#demo-shade').addEventListener('click', () => closeOverlay());
$('#preview-website').addEventListener('click', () => { closeOverlay(); $('#main').scrollIntoView({ behavior: 'smooth' }); });
document.addEventListener('click', event => {
  const target = event.target.closest('button');
  if (!target) return;
  if (target.dataset.open) { openOverlay(target.dataset.open); if (target.classList.contains('new-chat')) goPage('projects'); }
  if (target.hasAttribute('data-close')) closeOverlay();
  if (target.dataset.chat) { selectChat(target.dataset.chat); if (overlay) closeOverlay(); }
  if (target.dataset.view) target.dataset.view === 'workspace' ? closeOverlay(false) : openOverlay(target.dataset.view);
  if (target.dataset.themeChoice) setSiteTheme(target.dataset.themeChoice, true);
  if (target.dataset.answer) {
    chats.notes.status = 'Ready when you are';
    chats.notes.body = `<div class="work-line">${icon('check')}<span class="work-verb">Saved</span><code>this-week.md</code></div><div class="terminal-result"><strong>A little clarity for the week.</strong><p>1. Finish the first draft.<br>2. Share it with someone you trust.<br>3. Leave a little room for the unexpected.</p>${target.dataset.answer === 'full' ? '<p>Start with the page on Monday. Gather feedback midweek.<br>Keep Friday open for the small improvements.</p>' : ''}</div>`;
    $('.chat-row[data-chat="notes"] .status-dot').className = 'status-dot green';
    $('.chat-row[data-chat="notes"] .status-dot').setAttribute('aria-label', 'Done');
    selectChat('notes');
    $('#terminal-content').tabIndex = -1;
    $('#terminal-content').focus({ preventScroll: true });
  }
});
$('.demo-tabs').addEventListener('keydown', event => {
  const tabs = $$('.demo-tab');
  const index = tabs.indexOf(document.activeElement);
  if (index < 0 || !['ArrowLeft', 'ArrowRight', 'Home', 'End'].includes(event.key)) return;
  event.preventDefault();
  const next = event.key === 'Home' ? 0 : event.key === 'End' ? tabs.length - 1 : (index + (event.key === 'ArrowRight' ? 1 : -1) + tabs.length) % tabs.length;
  tabs[next].focus();
  tabs[next].click();
});
document.addEventListener('keydown', event => {
  if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === 'k') { event.preventDefault(); overlay === 'command' ? closeOverlay() : openOverlay('command'); }
  if ((event.metaKey || event.ctrlKey) && event.target.closest('#demo-window')) {
    if (event.key.toLowerCase() === 'p') { event.preventDefault(); openOverlay('command'); goPage('projects'); }
    if (event.shiftKey && event.key.toLowerCase() === 'h') { event.preventDefault(); openOverlay('command'); goPage('chats'); }
  }
  if (event.key === 'Escape' && overlay) { event.preventDefault(); closeOverlay(); }
});
function setSiteTheme(value, persist = false) {
  document.documentElement.dataset.theme = value;
  $$('[data-theme-choice]').forEach(button => button.setAttribute('aria-pressed', String(button.dataset.themeChoice === value)));
  $('meta[name="theme-color"]').content = value === 'dark' ? '#18191b' : '#f7f6f2';
  if (persist) { try { localStorage.setItem('diri-site-theme', value); } catch {} }
}
try { const saved = localStorage.getItem('diri-site-theme'); if (['light', 'dark'].includes(saved)) setSiteTheme(saved); } catch {}
selectChat(currentChat);
