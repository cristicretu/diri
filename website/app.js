const $ = (selector, root = document) => root.querySelector(selector);
const $$ = (selector, root = document) => [...root.querySelectorAll(selector)];
// Programmatic focus restoration should follow how the preview is being used.
const product = $('.product');
const agentSwitcher = $('.compatible');
function setInputMethod(method) {
  product.dataset.inputMethod = method;
  agentSwitcher.dataset.inputMethod = method;
}
setInputMethod('pointer');
document.addEventListener('pointerdown', () => setInputMethod('pointer'), true);
document.addEventListener('keydown', () => setInputMethod('keyboard'), true);
const icon = name => `<span class="icon" data-icon="${name}" aria-hidden="true"></span>`;
function paintIcons(root = document) {
  $$('[data-icon], [data-agent]', root).forEach(el => {
    const folder = el.dataset.agent ? 'brand' : 'icons';
    el.style.setProperty('--icon', `url("assets/${folder}/${el.dataset.agent || el.dataset.icon}.svg")`);
    el.setAttribute('aria-hidden', 'true');
  });
}
const chats = {
  website: { title: 'Build the Diri website', agent: 'codex', name: 'Codex', prompt: 'Make a website for Diri using the app’s design system.' },
  notes: { title: 'Weekly plan', agent: 'claude', name: 'Claude Code', prompt: 'Create a weekly plan from these notes.' },
  details: { title: 'Keyboard navigation', agent: 'cursor', name: 'Cursor', prompt: 'Fix keyboard navigation in the command menu.' },
  weekend: { title: 'Travel map', agent: 'gemini', name: 'Gemini', prompt: 'Build a map of places to visit.' }
};
let notesAnswer = null;
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
  const preview = agentPreviews[chat.agent].render(chat, notesAnswer);
  $('.terminal-body').dataset.cli = chat.agent;
  $('#terminal-content').innerHTML = `<div class="terminal-scene">${preview.html}</div>`;
  $('.terminal-composer').innerHTML = preview.composer;
  $('.terminal-footer').innerHTML = preview.footer;
  $('.changes-content').innerHTML = preview.changes;
  $('.diff-count').textContent = `+${$$('.changes-content .added').length}`;
  $$('.agent-switch').forEach(button => button.setAttribute('aria-pressed', String(button.dataset.chat === id)));
  $('.new-chat small').textContent = chat.name;
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
  $$('.links-trigger, .notification-trigger').forEach(button => button.setAttribute('aria-expanded', 'false'));
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
  $$('.links-trigger, .notification-trigger').forEach(button => button.setAttribute('aria-expanded', String(button.dataset.open === kind)));
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
    notesAnswer = target.dataset.answer;
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
