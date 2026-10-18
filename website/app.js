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
// Icon images come from style.css ([data-icon] / [data-agent] rules); this only keeps them out of the accessibility tree.
function paintIcons(root = document) {
  $$('[data-icon], [data-agent]', root).forEach(el => el.setAttribute('aria-hidden', 'true'));
}
const chats = {
  release: { title: 'Ship the 0.9 release', agent: 'claude', name: 'Claude Code', view: 'swarm', prompt: 'Use diri to ship 0.9: split the work across agents and report back.' },
  website: { title: 'Build the Diri website', agent: 'codex', name: 'Codex', prompt: 'Make a website for Diri using the app’s design system.' },
  notes: { title: 'Weekly plan', agent: 'claude', name: 'Claude Code', prompt: 'Create a weekly plan from these notes.' },
  details: { title: 'Keyboard navigation', agent: 'cursor', name: 'Cursor', prompt: 'Fix keyboard navigation in the command menu.' },
  weekend: { title: 'Travel map', agent: 'gemini', name: 'Gemini', prompt: 'Build a map of places to visit.' }
};
let notesAnswer = null;
let currentChat = 'release';
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
  const preview = agentPreviews[chat.view || chat.agent].render(chat, notesAnswer);
  $('.terminal-body').dataset.cli = chat.agent;
  $('#terminal-content').innerHTML = `<div class="terminal-scene">${preview.html}</div>`;
  $('#terminal-content').classList.toggle('tail', Boolean(preview.tail));
  $('.terminal-composer').innerHTML = preview.composer;
  $('.terminal-footer').innerHTML = preview.footer;
  $('.changes-content').innerHTML = preview.changes;
  $('.diff-count').textContent = `+${$$('.changes-content .added').length}`;
  $$('.agent-switch').forEach(button => button.setAttribute('aria-pressed', String(button.dataset.chat === id)));
  swarmScene.shown(id === 'release');
  const newAgent = $('.new-chat-agent');
  newAgent.dataset.agent = chat.agent;
  newAgent.className = `agent-logo new-chat-agent ${chat.agent}`;
  paintIcons();
}
function closeOverlay(restoreFocus = true) {
  $$('.floating-panel').forEach(el => { el.hidden = true; });
  $('#demo-shade').hidden = true;
  overlay = null;
  $$('.links-trigger, .notification-trigger').forEach(button => button.setAttribute('aria-expanded', 'false'));
  $('.app-sidebar').inert = false;
  $('.app-main').inert = false;
  $('.changes-panel').inert = false;
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
  if (kind === 'command') { goPage('commands'); $('#palette-input').focus({ preventScroll: true }); }
  else $('button, a', panel)?.focus({ preventScroll: true });
}
const chatItem = (id) => ({ label: chats[id].title, agent: chats[id].agent, action: () => { selectChat(id); closeOverlay(); } });
function itemsForPage() {
  if (page === 'chats') return Object.keys(chats).map(chatItem);
  if (page === 'projects') return [{ label: 'Diri', icon: 'folder', action: () => { selectChat('release'); closeOverlay(); } }, { label: 'Experiments', icon: 'folder', action: () => { selectChat('weekend'); closeOverlay(); } }];
  return [chatItem('release'), chatItem('website'), chatItem('notes'), { label: 'Search chats', icon: 'search', hint: '⇧⌘H', action: () => goPage('chats') }, { label: 'Open project', icon: 'folder', hint: '⌘P', action: () => goPage('projects') }, { label: 'Notifications', icon: 'bell', action: () => openOverlay('notifications') }];
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
document.addEventListener('keydown', event => {
  if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === 'k') { event.preventDefault(); overlay === 'command' ? closeOverlay() : openOverlay('command'); }
  if ((event.metaKey || event.ctrlKey) && event.target.closest('#demo-window')) {
    if (event.key.toLowerCase() === 'p') { event.preventDefault(); openOverlay('command'); goPage('projects'); }
    if (event.shiftKey && event.key.toLowerCase() === 'h') { event.preventDefault(); openOverlay('command'); goPage('chats'); }
  }
  if (event.key === 'Escape' && overlay) { event.preventDefault(); closeOverlay(); }
});

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

// The download menu closes like a menu: outside click or Escape.
const downloadMenu = $('.download-options');
// Capture-phase pointerdown runs before any other handler can swallow the click.
document.addEventListener('pointerdown', event => { if (downloadMenu.open && !downloadMenu.contains(event.target)) downloadMenu.open = false; }, true);
downloadMenu.addEventListener('focusout', event => { if (event.relatedTarget && !downloadMenu.contains(event.relatedTarget)) downloadMenu.open = false; });
document.addEventListener('keydown', event => { if (event.key === 'Escape' && downloadMenu.open) { downloadMenu.open = false; $('summary', downloadMenu).focus(); } });

// The masthead gains a little body once content scrolls beneath it.
const masthead = $('.site-header');
const syncMasthead = () => masthead.classList.toggle('scrolled', scrollY > 8);
addEventListener('scroll', syncMasthead, { passive: true });
syncMasthead();

// The swarm demo: the first frame is already mid-run (three helpers working), so the window is never empty.
// When the window scrolls into view the helpers finish one by one, then it rests on its final state.
const swarmScene = (() => {
  const reduced = matchMedia('(prefers-reduced-motion: reduce)').matches;
  const steps = ['prompt', 'plan', 'spawn', 'notes', 'done'];
  let state = 'idle';
  let timers = [];
  const later = (ms, fn) => timers.push(setTimeout(fn, ms));
  const setRow = (row, status) => {
    const cell = $('.chat-state', row);
    if (!cell) return;
    cell.className = `chat-state ${status === 'done' ? 'completed' : 'working'}`;
    cell.setAttribute('aria-label', status === 'done' ? 'Completed' : 'Working');
    $('.icon', cell).dataset.icon = status === 'done' ? 'check' : 'working-0';
    paintIcons(row);
  };
  const setChild = (n, status) => {
    const line = $(`.swarm-child[data-child="${n}"] .swarm-state`);
    if (line) { line.className = `swarm-state ${status === 'done' ? 'done' : 'run'}`; line.textContent = status === 'done' ? '✓' : '◌'; }
    const row = $$('[data-child-row]')[n - 1];
    if (row) setRow(row, status);
  };
  const reveal = step => {
    const line = $(`[data-step="${step}"]`);
    if (!line?.classList.contains('scene-hidden')) return;
    line.classList.remove('scene-hidden');
    if (state === 'playing') line.classList.add('scene-in');
  };
  const footer = text => { const f = $('.swarm-footer'); if (f) f.textContent = text; };
  function finish() {
    timers.forEach(clearTimeout); timers = [];
    state = 'done';
    $('.product').classList.remove('scene-running');
    steps.forEach(reveal);
    [1, 2, 3].forEach(n => setChild(n, 'done'));
    setRow($('.chat-row[data-chat="release"]'), 'done');
    footer('3 agents finished');
  }
  function prepare() {
    ['notes', 'done'].forEach(step => $(`[data-step="${step}"]`)?.classList.add('scene-hidden'));
    [1, 2, 3].forEach(n => setChild(n, 'run'));
    setRow($('.chat-row[data-chat="release"]'), 'run');
    footer('3 agents running');
  }
  function play() {
    state = 'playing';
    $('.product').classList.add('scene-running');
    let t = 1400;
    later(t, () => { setChild(1, 'done'); footer('2 agents running'); });
    later(t += 700, () => reveal('notes'));
    later(t += 1500, () => { setChild(2, 'done'); footer('1 agent running'); });
    later(t += 1300, () => { setChild(3, 'done'); reveal('done'); footer('3 agents finished'); });
    later(t += 400, finish);
  }
  let visible = false;
  const observer = new IntersectionObserver(entries => {
    visible = entries.some(entry => entry.isIntersecting);
    if (visible && state === 'armed') play();
  }, { threshold: 0.3 });
  observer.observe($('#demo-window'));
  return {
    shown(isSwarm) {
      if (!isSwarm) { if (state === 'playing' || state === 'armed') state = 'done'; timers.forEach(clearTimeout); timers = []; $('.product').classList.remove('scene-running'); return; }
      if (state === 'idle' && !reduced) { state = 'armed'; prepare(); if (visible) play(); }
      else finish();
    }
  };
})();
selectChat(currentChat);

