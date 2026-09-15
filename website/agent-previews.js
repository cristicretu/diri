// Gemini ASCII mark: Copyright 2025 Google LLC, Apache-2.0.
// Source: google-gemini/gemini-cli, packages/cli/src/ui/components/AsciiArt.ts
const geminiAscii = `   █████████  ██████████ ██████   ██████ █████ ██████   █████ █████
  ███░░░░░███░░███░░░░░█░░██████ ██████ ░░███ ░░██████ ░░███ ░░███
 ███     ░░░  ░███  █ ░  ░███░█████░███  ░███  ░███░███ ░███  ░███
░███          ░██████    ░███░░███ ░███  ░███  ░███░░███░███  ░███
░███    █████ ░███░░█    ░███ ░░░  ░███  ░███  ░███ ░░██████  ░███
░░███  ░░███  ░███ ░   █ ░███      ░███  ░███  ░███  ░░█████  ░███
 ░░█████████  ██████████ █████     █████ █████ █████  ░░█████ █████
  ░░░░░░░░░  ░░░░░░░░░░ ░░░░░     ░░░░░ ░░░░░ ░░░░░    ░░░░░ ░░░░░`;

// Terminal-specific presentation for the website's illustrative sessions.
// UI references are listed in README.md; none of these views runs an agent.
const agentPreviews = (() => {
  const esc = value => value.replaceAll('&', '&amp;').replaceAll('<', '&lt;').replaceAll('>', '&gt;');
  const mark = name => `<span class="agent-logo ${name}" data-agent="${name}" aria-hidden="true"></span>`;
  const tool = (name, file, detail) => `<div class="claude-tool"><span class="claude-bullet">●</span><div><strong>${name}</strong><span>(${file})</span><small>└ ${detail}</small></div></div>`;
  const changes = (file, lines) => `<div class="diff-file"><span class="icon" data-icon="code"></span>${file}</div><div class="diff-lines">${lines.map((line, i) => `<div class="${line.startsWith('+') ? 'added' : ''}"><span>${line.startsWith('+') ? '+' : i + 1}</span><code>${esc(line.replace(/^\+/, ''))}</code></div>`).join('')}</div>`;
  const command = (symbol, placeholder) => `<span class="composer-prefix">${symbol}</span><span>${placeholder}</span><span class="terminal-cursor" aria-hidden="true"></span>`;
  return {
    codex: {
      render(chat) {
        return {
          html: `<div class="codex-welcome"><strong><span>›_</span> OpenAI Codex</strong><div><span>model:</span> gpt-6 <small>/model to change</small></div><div><span>directory:</span> ~/fun/dirijor</div></div><div class="codex-user">› ${chat.prompt}</div><p class="cli-response">I’ll match the app’s layout and keep the terminal opaque.</p><div class="codex-explored"><strong>Explored</strong><div>└ Read <span>index.html, style.css</span></div></div><div class="codex-explored"><strong>Edited</strong><div>└ <span>website/style.css</span> <em>+5</em></div></div><div class="cli-divider">Worked for 42s</div><div class="cli-summary"><p>Updated the preview:</p><p>— Native window proportions<br>— Translucent sidebars and toolbar<br>— Rosé Pine shader background</p><button class="result-link" data-open="links">Open preview ↗</button></div>`,
          composer: command('›', 'Ask a question or describe a task'),
          footer: '<span>gpt-6</span><span>~/fun/dirijor</span>',
          changes: changes('website/style.css', [':root {','+  --background: #191724;','+  --accent: #ebbcba;','}', '', '.terminal-body {', '+  background: #191724;', '}', '', '.app-window {', '+  aspect-ratio: 16 / 10;', '+  backdrop-filter: blur(16px);', '}'])
        };
      }
    },
    claude: {
      render(chat, answer) {
        const reply = answer
          ? `${tool('Write', 'this-week.md', 'Wrote weekly plan')}<div class="claude-answer"><span class="claude-bullet">●</span><div><strong>Plan saved.</strong><p>1. Finish the first draft<br>2. Collect feedback<br>3. Refine the design</p>${answer === 'full' ? '<p>Monday: first draft<br>Wednesday: feedback<br>Friday: revisions</p>' : ''}</div></div>`
          : `${tool('Read', 'notes.md', 'Read 24 lines')}<div class="claude-answer"><span class="claude-bullet">●</span><div><p>How much detail should the plan include?</p><div class="claude-choices"><button data-answer="short"><span>1.</span> Summary</button><button data-answer="full"><span>2.</span> Detailed plan</button></div></div></div>`;
        return {
          html: `<div class="claude-welcome"><pre class="claude-mascot" aria-hidden="true"> ▐▛███▜▌\n▝▜█████▛▘\n  ▘▘ ▝▝</pre><div><strong>Claude Code</strong><span>Sonnet</span><small>~/fun/dirijor</small></div></div><div class="claude-user">❯ ${chat.prompt}</div>${reply}`,
          composer: command('❯', 'Try "summarize this project"'),
          footer: '<span>? for shortcuts</span><span>Sonnet</span>',
          changes: changes('this-week.md', ['+# This week', '+', '+- Finish the first draft', '+- Collect feedback', '+- Refine the design'])
        };
      }
    },
    cursor: {
      render(chat) {
        return {
          html: `<div class="cursor-welcome">${mark('cursor')}<div><strong>Cursor Agent</strong><small>~/fun/dirijor <span>main</span></small></div></div><div class="cursor-user"><span>→</span>${chat.prompt}</div><p class="cursor-thinking">I’ll check the focus handling and key bindings.</p><div class="cursor-tool"><span>Read</span><code>navigation.rs</code><small>128 lines</small></div><div class="cursor-tool"><span>Edit</span><code>navigation.rs</code><small class="cursor-gain">+3</small></div><div class="cursor-patch"><span>+ Key::Up =&gt; select_previous(),</span><span>+ Key::Down =&gt; select_next(),</span><span>+ Key::Escape =&gt; close_menu(),</span></div><p class="cursor-result">Arrow keys move the selection. Escape closes the menu and restores focus.</p><button class="result-link" data-open="command">Open commands ↗</button>`,
          composer: command('→', 'Add a follow-up'),
          footer: '<span>Auto</span><span>/ commands &nbsp; @ files &nbsp; ! shell</span>',
          changes: changes('navigation.rs', ['match key {', '+  Key::Up => select_previous(),', '+  Key::Down => select_next(),', '+  Key::Escape => close_menu(),', '  _ => {}', '}'])
        };
      }
    },
    gemini: {
      render(chat) {
        return {
          html: `<pre class="gemini-ascii" aria-label="Gemini CLI">${esc(geminiAscii)}</pre><div class="gemini-user"><span>›</span>${chat.prompt}</div><div class="gemini-tool"><div><span>✓</span><strong>WriteFile</strong><code>src/Map.tsx</code></div><p>Created map with saved places and notes</p></div><div class="gemini-tool"><div><span>✓</span><strong>Shell</strong><code>npm run build</code></div><p>Build completed successfully</p></div><div class="gemini-answer"><span>✦</span><p>The map is ready. Add a place to save it, or select a pin to edit its note.</p></div>`,
          composer: command('>', 'Type your message or @path/to/file'),
          footer: '<span>~/travel-map</span><span>Auto</span>',
          changes: changes('src/Map.tsx', ['+export function Map() {', '+  return (', '+    <MapView>', '+      <SavedPlaces />', '+    </MapView>', '+  );', '+}'])
        };
      }
    }
  };
})();
