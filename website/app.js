const $ = (selector, root = document) => root.querySelector(selector);
const $$ = (selector, root = document) => [...root.querySelectorAll(selector)];

// Homebrew casks are macOS only: the brew lines show for Mac visitors (not touch iPads) and stay hidden for everyone else.
const onMac = /mac/i.test(navigator.userAgentData?.platform ?? navigator.platform ?? '') && navigator.maxTouchPoints < 2;
$$('[data-brew]').forEach(line => { line.hidden = !onMac; });

// Copy the brew command; the label confirms, or says so when the clipboard is unavailable.
$$('.brew-copy').forEach(button => button.addEventListener('click', async () => {
  try { await navigator.clipboard.writeText(button.dataset.copy); button.textContent = 'Copied'; }
  catch { button.textContent = 'Copy failed'; }
  setTimeout(() => { button.textContent = 'Copy'; }, 1600);
}));
// Icon images come from style.css ([data-icon] / [data-agent] rules); this only keeps them out of the accessibility tree.
$$('[data-icon], [data-agent]').forEach(el => el.setAttribute('aria-hidden', 'true'));

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
