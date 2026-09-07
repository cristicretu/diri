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
  website: { title: 'Build the Diri website', agent: 'codex', name: 'Codex', status: 'gpt-6', prompt: 'Make a website for Diri using the app’s design system.', body: `<p class="cli-response">I’ll use the app’s layout, icons, and Rosé Pine theme.</p><div class="cli-tool">${icon('check')}<strong>Read</strong><code>website/index.html</code></div><div class="cli-tool">${icon('check')}<strong>Updated</strong><code>website/style.css</code></div><div class="cli-tool">${icon('check')}<strong>Added</strong><code>website/mesh.js</code></div><div class="cli-summary"><p>The preview now has:</p><p>— A Rosé Pine mesh background<br>— Translucent sidebar and terminal<br>— A separate changes panel</p><button class="result-link" data-open="links">Open preview ${icon('external-link')}</button></div><div class="cli-divider">Changes ready to review</div>` },
  notes: { title: 'Weekly plan', agent: 'codex', name: 'Codex', status: 'Needs input', prompt: 'Create a weekly plan from these notes.', body: `<div class="work-line">${icon('check')}<span class="work-verb">Read</span><code>monday-notes.md · ideas.md · next-up.md</code></div><div class="terminal-result"><strong>Notes reviewed.</strong><p>How much detail should the plan include?</p><div class="question-options"><button data-answer="short">Summary</button><button data-answer="full">Detailed plan</button></div></div>` },
  details: { title: 'Keyboard navigation', agent: 'cursor', name: 'Cursor', status: 'Working', prompt: 'Fix keyboard navigation in the command menu.', body: `<div class="work-line">${icon('check')}<span class="work-verb">Checked</span><code>keyboard navigation and focus</code></div><div class="work-line">${icon('check')}<span class="work-verb">Refined</span><code>the command menu transition</code></div><div class="terminal-result"><strong>Navigation updated.</strong><p>Back navigation and keyboard shortcuts are ready to test.</p><button class="result-link" data-open="command">Open commands ${icon('chevron-right')}</button></div>` },
  weekend: { title: 'Travel map', agent: 'gemini', name: 'Gemini', status: 'Idle', prompt: 'Build a map of places to visit.', body: `<div class="terminal-result"><strong>Map created.</strong><p>Add places, notes, and map pins.</p></div>` }
};
let currentChat = 'website';
let overlay = null;
let previousFocus;
let page = 'commands';
let selected = 0;
let matches = [];
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
  $('#terminal-content').innerHTML = `<div class="terminal-scene"><div class="prompt-line"><span>›</span><span>${chat.prompt}</span></div>${chat.body}</div>`;
  paintIcons();
}
function setTab(view) {
  $$('.demo-tab').forEach(tab => { const active = tab.dataset.view === view; tab.classList.toggle('active', active); tab.setAttribute('aria-selected', String(active)); tab.tabIndex = active ? 0 : -1; });
  $('#demo-window').setAttribute('aria-labelledby', `tab-${view}`);
}
function closeOverlay(restoreFocus = true) {
  $$('.floating-panel').forEach(el => { el.hidden = true; });
  $('#demo-shade').hidden = true;
  overlay = null;
  $('.app-sidebar').inert = false;
  $('.app-main').inert = false;
  $('.changes-panel').inert = false;
  setTab('workspace');
  if (restoreFocus && previousFocus?.isConnected) previousFocus.focus({ preventScroll: true });
}
function openOverlay(kind) {
  if (!overlay) previousFocus = document.activeElement;
  closeOverlay(false);
  overlay = kind;
  $('.app-sidebar').inert = true;
  $('.app-main').inert = true;
  $('.changes-panel').inert = true;
  $('#demo-shade').hidden = false;
  const panel = $(kind === 'command' ? '#palette' : `#${kind}-panel`);
  panel.hidden = false;
  setTab(kind === 'notifications' ? 'workspace' : kind);
  if (kind === 'command') { goPage('commands'); $('#palette-input').focus({ preventScroll: true }); }
  else $('button, a', panel)?.focus({ preventScroll: true });
}
const chatItem = (id) => ({ label: chats[id].title, agent: chats[id].agent, action: () => { selectChat(id); closeOverlay(); } });
function itemsForPage() {
  if (page === 'chats') return Object.keys(chats).map(chatItem);
  if (page === 'projects') return [{ label: 'Diri', icon: 'folder', action: () => { selectChat('website'); closeOverlay(); } }, { label: 'Experiments', icon: 'folder', action: () => { selectChat('weekend'); closeOverlay(); } }];
  return [chatItem('website'), chatItem('notes'), { label: 'Search chats', icon: 'search', hint: '⇧⌘H', action: () => goPage('chats') }, { label: 'Open project', icon: 'folder', hint: '⌘P', action: () => goPage('projects') }, { label: 'Notifications', icon: 'bell', action: () => openOverlay('notifications') }];
}
function goPage(next) {
  page = next;
  selected = 0;
  $('#palette-input').value = '';
  $('#palette-input').placeholder = { commands: 'Search chats or run a command…', chats: 'Search chats…', projects: 'Open a project…' }[page];
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
    row.innerHTML = `${item.agent ? `<span class="agent-logo ${item.agent}" data-agent="${item.agent}"></span>` : icon(item.icon)}<span>${item.label}</span>${item.hint ? `<small>${item.hint}</small>` : ''}`;
    row.addEventListener('mousedown', event => event.preventDefault());
    row.addEventListener('click', item.action);
    row.addEventListener('pointermove', () => highlight(i));
    list.append(row);
  });
  if (!matches.length) { const empty = document.createElement('p'); empty.className = 'palette-empty'; empty.textContent = 'No results.'; list.append(empty); }
  paintIcons(list);
  highlight(selected);
}
function highlight(index) {
  selected = index;
  $$('.palette-option').forEach((row, i) => row.setAttribute('aria-selected', String(i === index)));
  const item = matches[index];
  if (item) $('#palette-input').setAttribute('aria-activedescendant', `command-${index}`);
  else $('#palette-input').removeAttribute('aria-activedescendant');
  const row = $(`#command-${index}`);
  const list = $('#palette-list');
  if (row && row.offsetTop < list.scrollTop) list.scrollTop = row.offsetTop;
  else if (row && row.offsetTop + row.offsetHeight > list.scrollTop + list.clientHeight) list.scrollTop = row.offsetTop + row.offsetHeight - list.clientHeight;
}
$('#palette-input').addEventListener('input', () => { selected = 0; renderPalette(); });
$('#palette-input').addEventListener('keydown', event => {
  if (['ArrowDown', 'ArrowUp'].includes(event.key)) { event.preventDefault(); if (matches.length) highlight((selected + (event.key === 'ArrowDown' ? 1 : -1) + matches.length) % matches.length); }
  if (event.key === 'Enter') { event.preventDefault(); matches[selected]?.action(); }
  if (event.key === 'Backspace' && !event.target.value && page !== 'commands') { event.preventDefault(); goPage('commands'); }
});
$('#palette-back').addEventListener('click', () => goPage('commands'));
$('#demo-shade').addEventListener('click', () => closeOverlay());
$('#preview-website').addEventListener('click', () => { closeOverlay(); $('#main').scrollIntoView({ behavior: 'smooth' }); });
document.addEventListener('click', event => {
  const target = event.target.closest('button');
  if (!target) return;
  if (target.dataset.open) { openOverlay(target.dataset.open); if (target.classList.contains('new-chat')) goPage('projects'); }
  if (target.hasAttribute('data-close')) closeOverlay();
  if (target.dataset.chat) { selectChat(target.dataset.chat); if (overlay) closeOverlay(); }
  if (target.dataset.view) target.dataset.view === 'workspace' ? closeOverlay(false) : openOverlay(target.dataset.view);
  if (target.dataset.answer) {
    chats.notes.status = 'Completed';
    chats.notes.body = `<div class="work-line">${icon('check')}<span class="work-verb">Saved</span><code>this-week.md</code></div><div class="terminal-result"><strong>Plan saved.</strong><p>1. Finish the first draft.<br>2. Collect feedback.<br>3. Refine the design based on feedback.</p>${target.dataset.answer === 'full' ? '<p>Start with the page on Monday. Gather feedback midweek.<br>Revise the page on Friday.</p>' : ''}</div>`;
    const state = $('.chat-row[data-chat="notes"] .chat-state');
    state.className = 'chat-state completed';
    state.setAttribute('aria-label', 'Completed');
    $('.icon', state).dataset.icon = 'check';
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
selectChat(currentChat);

function toggleChanges() {
  const hidden = $('#demo-window').classList.toggle('changes-hidden');
  $('#toggle-changes').setAttribute('aria-expanded', String(!hidden));
}
$('#toggle-changes').addEventListener('click', toggleChanges);
$('#close-changes').addEventListener('click', () => { toggleChanges(); $('#toggle-changes').focus({preventScroll:true}); });
$('#toggle-sidebar').addEventListener('click', () => {
  $('#demo-window').classList.toggle('sidebar-collapsed');
  $('#toggle-sidebar').setAttribute('aria-label', $('#demo-window').classList.contains('sidebar-collapsed') ? 'Expand sidebar' : 'Collapse sidebar');
});
