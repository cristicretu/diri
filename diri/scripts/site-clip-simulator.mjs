// Draws the Simulator windows site-clips.sh lays over the computer-use clip:
// one phone per agent, each with that agent's cursor moving, clicking and
// typing at the same time, in step with the transcript in the pane.
//
//   node scripts/site-clip-simulator.mjs <recorded clip dir> <out dir> <width> <height>
//
// Reads the clip's frames.txt (frame, hold) and marks.txt (step, ms from the
// start) and writes <out>/overlay.txt with one line per overlay frame: the
// recorded frame under it, a transparent PNG the size of the clip, and how
// long it holds. Needs Google Chrome (CHROME=<path> to use another build).
import { spawn } from 'node:child_process';
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

// The clip's size in CSS pixels; Chrome renders at 2x like the recording.
const [clip, out, W, H] = process.argv.slice(2).map((arg, i) => (i < 2 ? arg : Number(arg)));
const lines = file => readFileSync(join(clip, file), 'utf8').trim().split('\n').map(l => l.split(/\s+/));
const frames = lines('frames.txt').map(([frame, hold]) => [frame, Number(hold)]);
const mark = Object.fromEntries(lines('marks.txt').map(([name, at]) => [name, Number(at)]));

// Each phone's story in ms. A phone opens just after its agent's tab is
// selected (the codex, pi and claude marks), so the window shows who opened
// it, and each tests something else: Codex signs in on the SE and finds the
// overlap the transcript reports, flagged when the children report back and
// fixed by the rebuild; Pi pages through the tour in dark mode; Claude Code
// turns on notifications through the system alert.
const devices = [
  { kind: 'signin', title: 'iPhone SE', agent: 'Codex', tag: '#e8e6ee', ink: '#17151f', x: W - 490, y: 72,
    open: mark.codex + 300, start: mark.codex + 700, email: 'sam@acme.dev', bug: true },
  { kind: 'tour', title: 'iPhone 17 Pro Max', agent: 'Pi', tag: '#7f8cff', ink: '#fff', x: W - 345, y: 100,
    open: mark.pi + 300, start: mark.pi + 700, dark: true },
  { kind: 'notify', title: 'iPhone 17 Pro', agent: 'Claude Code', tag: '#d97757', ink: '#fff', x: W - 200, y: 128,
    open: mark.claude + 300, start: mark.claude + 700 },
];
const FIELD = [70, 223], BUTTON = [98, 263], REST = [150, 330], AWAY = [112, 292];
const NEXT = [98, 325], NOTIFY = [98, 315], ALLOW = [138, 254], CLEAR = [60, 270];
const GLIDE = 380, PER_KEY = 45, PRESS = 140, TOUR_STEP = 650;
const ease = t => (t < 0.5 ? 2 * t * t : 1 - (-2 * t + 2) ** 2 / 2);
const clamp = v => Math.max(0, Math.min(1, v));
const lerp = (a, b, t) => [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t];
// Where a cursor is at `t` along `path`, a list of [ms, point] stops it
// glides between.
const along = (path, t) => {
  let at = path[0][1];
  for (let i = 1; i < path.length; i++) if (t >= path[i][0]) at = lerp(path[i - 1][1], path[i][1], ease(clamp((t - path[i][0]) / GLIDE)));
  return at;
};
const ripple = (clicks, t) => clicks.map(at => (t - at) / 200).find(p => p >= 0 && p < 1) ?? null;

function signin(d, t) {
  const typedAt = d.start + GLIDE + PRESS;
  const toButton = typedAt + d.email.length * PER_KEY + 120;
  const pressAt = toButton + GLIDE;
  const welcomeAt = pressAt + PRESS;
  const keys = t >= typedAt ? Math.floor((t - typedAt) / PER_KEY) + 1 : 0;
  return {
    cursor: along([[0, REST], [d.start, FIELD], [toButton, BUTTON], [welcomeAt + 500, AWAY]], t),
    ripple: ripple([d.start + GLIDE, pressAt], t),
    focus: t >= d.start + GLIDE && t < welcomeAt,
    typed: d.email.slice(0, keys),
    pressed: t >= pressAt && t < welcomeAt,
    screen: t >= welcomeAt ? 'welcome' : 'signin',
    fixed: t >= mark.done,
    ring: t >= mark.wait ? (t >= mark.done ? '#22c55e' : '#ef4444') : null,
    ringOpacity: clamp((t - mark.wait) / 120),
  };
}

function tour(d, t) {
  const taps = [0, 1, 2].map(i => d.start + GLIDE + i * TOUR_STEP);
  const page = taps.filter(at => t >= at + PRESS).length;
  return {
    cursor: along([[0, REST], [d.start, NEXT], [taps[2] + 600, CLEAR]], t),
    ripple: ripple(taps, t),
    pressed: taps.some(at => t >= at && t < at + PRESS),
    screen: 'tour',
    page: Math.min(page, 2),
    started: page > 2,
  };
}

function notify(d, t) {
  const tapAt = d.start + GLIDE;
  const alertAt = tapAt + PRESS;
  const allowAt = alertAt + 500 + GLIDE;
  return {
    cursor: along([[0, REST], [d.start, NOTIFY], [alertAt + 500, ALLOW], [allowAt + 700, CLEAR]], t),
    ripple: ripple([tapAt, allowAt], t),
    pressed: t >= tapAt && t < alertAt,
    screen: 'notify',
    alert: t >= alertAt && t < allowAt + PRESS,
    allowPressed: t >= allowAt && t < allowAt + PRESS,
    on: t >= allowAt + PRESS,
  };
}

const stories = { signin, tour, notify };
function phone(d, t) {
  const show = clamp((t - d.open) / 160);
  return { show, ...stories[d.kind](d, t), ...(t < d.open + 200 ? { cursor: null, ripple: null } : {}) };
}

const page = `<!doctype html><meta charset="utf-8"><style>
* { box-sizing: border-box; margin: 0; }
html, body { width: ${W}px; height: ${H}px; background: transparent; overflow: hidden; font-family: -apple-system, "SF Pro Text", sans-serif; -webkit-font-smoothing: antialiased; }
.stage { position: absolute; scale: .82; transform-origin: 0 0; }
.win { position: absolute; left: 0; top: 0; width: 220px; height: 468px; border-radius: 12px; background: #2a2838; border: 1px solid #ffffff1f; box-shadow: 0 18px 50px #00000099, 0 2px 8px #00000066; transform-origin: 50% 40%; }
.bar { height: 26px; display: flex; align-items: center; gap: 6px; padding: 0 10px; }
.bar i { width: 9px; height: 9px; border-radius: 50%; }
.bar span { margin-left: 8px; font-size: 9.5px; font-weight: 600; color: #d8d3e4; }
.phone { position: absolute; left: 6px; top: 26px; width: 208px; height: 436px; border-radius: 30px; background: #0b0b0f; padding: 6px; }
.screen { position: relative; width: 196px; height: 424px; border-radius: 25px; overflow: hidden; background: #f6f6f9; color: #15131c; --sub: #6e6a7a; --field: #fff; --edge: #d9d7e1; --next: #15131c; --next-ink: #fff; }
.screen.dark { background: #0e0e12; color: #f4f2f8; --sub: #9a96a6; --field: #1b1a22; --edge: #34323f; --next: #f4f2f8; --next-ink: #15131c; }
.abs { position: absolute; left: 0; right: 0; text-align: center; }
.island { position: absolute; top: 9px; left: 70px; width: 56px; height: 16px; border-radius: 9px; background: #000; }
.time { position: absolute; top: 10px; left: 22px; font-size: 9.5px; font-weight: 600; }
.logo { position: absolute; top: 92px; left: 76px; width: 44px; height: 44px; border-radius: 12px; background: linear-gradient(135deg, #6d5dfc, #c06cf2); }
h1 { font-size: 16px; font-weight: 650; letter-spacing: -.01em; }
.sub { font-size: 9.5px; color: var(--sub); }
.field { position: absolute; top: 208px; left: 18px; width: 160px; height: 30px; border-radius: 9px; border: 1px solid var(--edge); background: var(--field); font-size: 10.5px; padding: 0 10px; display: flex; align-items: center; color: var(--sub); }
.field.focus { border-color: #6d5dfc; box-shadow: 0 0 0 3px #6d5dfc26; color: inherit; }
.field.focus::after { content: ''; width: 1px; height: 13px; margin-left: 1px; background: #6d5dfc; }
.button { position: absolute; top: 248px; left: 18px; width: 160px; height: 30px; border-radius: 9px; background: #6d5dfc; color: #fff; font-size: 10.5px; font-weight: 600; display: flex; align-items: center; justify-content: center; }
.button.pressed { background: #5847e0; transform: scale(.97); }
.art { position: absolute; top: 74px; left: 40px; width: 116px; height: 116px; border-radius: 50%; background: radial-gradient(circle at 35% 30%, #ffffffb3, #fff0 40%), linear-gradient(135deg, #8b7dff, #f08bc8); }
.next { position: absolute; top: 310px; left: 18px; width: 160px; height: 30px; border-radius: 9px; background: var(--next); color: var(--next-ink); font-size: 10.5px; font-weight: 600; display: flex; align-items: center; justify-content: center; }
.dots { position: absolute; left: 0; right: 0; display: flex; justify-content: center; gap: 5px; }
.dots i { width: 6px; height: 6px; border-radius: 50%; background: #c9c6d3; }
.skip { font-size: 10px; font-weight: 500; color: var(--sub); }
.ring { position: absolute; left: 50px; width: 96px; border-radius: 10px; border: 2px solid; }
.dots i { transition: none; } .dots i.on, .dots:not(:has(.on)) i:first-child { width: 14px; border-radius: 3px; background: #6d5dfc; }
.next.pressed { transform: scale(.97); opacity: .85; }
.next.on { background: #22c55e; color: #fff; }
.bell { position: absolute; top: 96px; left: 62px; width: 72px; height: 72px; border-radius: 22px; display: grid; place-items: center; font-size: 34px; background: linear-gradient(135deg, #ffb36b, #d93a6a); }
.dim { position: absolute; inset: 0; background: #0006; }
.alert { position: absolute; top: 150px; left: 23px; width: 150px; border-radius: 13px; background: #f2f1f6f2; color: #15131c; text-align: center; overflow: hidden; box-shadow: 0 10px 30px #0005; }
.alert b { display: block; padding: 12px 12px 3px; font-size: 9.5px; line-height: 1.3; }
.alert p { padding: 0 12px 10px; font-size: 8.5px; color: #6e6a7a; }
.alert .choices { display: grid; grid-template-columns: 1fr 1fr; border-top: 1px solid #d9d7e1; }
.alert span { padding: 8px 0; font-size: 10px; color: #3478f6; }
.alert span + span { border-left: 1px solid #d9d7e1; font-weight: 600; }
.alert span.pressed { background: #0000000f; }
.cursor { position: absolute; left: 0; top: 0; }
.cursor b { position: absolute; left: 13px; top: 17px; padding: 2px 6px; border-radius: 6px; font-size: 9px; font-weight: 600; white-space: nowrap; box-shadow: 0 2px 6px #0006; }
.ripple { position: absolute; width: 26px; height: 26px; margin: -13px 0 0 -13px; border-radius: 50%; border: 2px solid; }
</style><body></body>
<script>
const signin = s => \`<div class="island"></div><div class="time">9:41</div><div class="logo"></div>
  <div class="abs" style="top:150px"><h1>Welcome to Acme</h1></div>
  <div class="abs sub" style="top:174px">Sign in with your work email.</div>
  <div class="field \${s.focus ? 'focus' : ''}">\${s.typed || (s.focus ? '' : 'Email')}</div>
  <div class="button \${s.pressed ? 'pressed' : ''}">Continue</div>\`;
// The page dots sit on top of Skip until the fix moves them up.
const welcome = s => \`<div class="island"></div><div class="time">9:41</div><div class="art"></div>
  <div class="abs" style="top:214px"><h1>Welcome</h1></div>
  <div class="abs sub" style="top:240px;line-height:1.45">Your workspace is ready.<br>Here’s a quick tour.</div>
  <div class="next">Next</div>
  <div class="dots" style="top:\${s.fixed ? 356 : 375}px"><i></i><i></i><i></i></div>
  <div class="abs skip" style="top:\${s.fixed ? 378 : 371}px">Skip</div>
  \${s.ring ? \`<div class="ring" style="top:\${s.fixed ? 348 : 363}px;height:\${s.fixed ? 50 : 26}px;border-color:\${s.ring};opacity:\${s.ringOpacity};box-shadow:0 0 0 4px \${s.ring}26"></div>\` : ''}\`;
const pages = [
  ['Welcome', 'Your workspace is ready.<br>Here’s a quick tour.', '#8b7dff, #f08bc8'],
  ['Agents everywhere', 'Start one on this Mac<br>or on any server.', '#5ec8ff, #7f8cff'],
  ['You’re all set', 'Notes, schedules and voice<br>are a tap away.', '#ffb36b, #d93a6a'],
];
const tour = s => {
  const [title, body, art] = pages[s.page];
  return \`<div class="island"></div><div class="time">9:41</div>
  <div class="art" style="background:radial-gradient(circle at 35% 30%, #ffffffb3, #fff0 40%), linear-gradient(135deg, \${art})"></div>
  <div class="abs" style="top:214px"><h1>\${title}</h1></div>
  <div class="abs sub" style="top:240px;line-height:1.45">\${body}</div>
  <div class="next \${s.pressed ? 'pressed' : ''}">\${s.page === 2 ? (s.started ? 'Let’s go ✓' : 'Get started') : 'Next'}</div>
  <div class="dots" style="top:356px">\${[0, 1, 2].map(i => \`<i class="\${i === s.page ? 'on' : ''}"></i>\`).join('')}</div>
  <div class="abs skip" style="top:378px">Skip</div>\`;
};
const notify = s => \`<div class="island"></div><div class="time">9:41</div>
  <div class="bell">🔔</div>
  <div class="abs" style="top:200px"><h1>Stay in the loop</h1></div>
  <div class="abs sub" style="top:226px;line-height:1.45">Hear when an agent finishes<br>or needs you.</div>
  <div class="next \${s.pressed ? 'pressed' : ''} \${s.on ? 'on' : ''}" style="top:300px">\${s.on ? 'Notifications on ✓' : 'Turn on notifications'}</div>
  \${s.alert ? \`<div class="dim"></div><div class="alert"><b>“Acme” Would Like to Send You Notifications</b><p>Alerts, sounds and badges.</p>
    <div class="choices"><span>Don’t Allow</span><span class="\${s.allowPressed ? 'pressed' : ''}">Allow</span></div></div>\` : ''}\`;
const screens = { signin, welcome, tour, notify };
window.draw = (devices, states) => {
  document.body.innerHTML = devices.map((d, i) => {
    const s = states[i];
    if (!s.show) return '';
    // Cursor positions are in screen points; the screen starts 12, 32 into the window.
    const cursor = s.cursor ? \`<div class="cursor" style="transform:translate(\${12 + s.cursor[0]}px,\${32 + s.cursor[1]}px)">
      \${s.ripple == null ? '' : \`<div class="ripple" style="border-color:\${d.tag};transform:translate(2px,2px) scale(\${0.4 + s.ripple});opacity:\${1 - s.ripple}"></div>\`}
      <svg width="16" height="20" viewBox="0 0 16 20"><path d="M1.5 1.5v15l4-3.8 2.6 6 2.6-1.1-2.6-5.9h5.6z" fill="\${d.tag}" stroke="\${d.dark ? '#000' : '#fff'}" stroke-width="1.4" stroke-linejoin="round"/></svg>
      <b style="background:\${d.tag};color:\${d.ink}">\${d.agent}</b></div>\` : '';
    return \`<div class="stage" style="left:\${d.x}px;top:\${d.y}px">
      <div class="win" style="opacity:\${s.show};transform:translateY(\${(1 - s.show) * 8}px) scale(\${0.97 + s.show * 0.03})">
        <div class="bar"><i style="background:#ff5f57"></i><i style="background:#febc2e"></i><i style="background:#28c840"></i><span>\${d.title}</span></div>
        <div class="phone"><div class="screen \${d.dark ? 'dark' : ''}">\${screens[s.screen](s)}</div></div>
      </div>\${cursor}</div>\`;
  }).join('');
};
</script>`;

const profile = mkdtempSync(join(tmpdir(), 'site-clip-chrome-'));
const chrome = spawn(process.env.CHROME || '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome',
  ['--headless=new', '--remote-debugging-port=0', `--user-data-dir=${profile}`, '--hide-scrollbars', 'about:blank'],
  { stdio: ['ignore', 'ignore', 'pipe'] });
const browser = await new Promise((resolve, reject) => {
  let err = '';
  chrome.stderr.on('data', d => { err += d; const m = err.match(/ws:\/\/\S+/); if (m) resolve(m[0]); });
  chrome.on('exit', () => reject(new Error(`Chrome exited: ${err}`)));
});
const port = new URL(browser).port;
const target = (await (await fetch(`http://127.0.0.1:${port}/json`)).json()).find(t => t.type === 'page');
const ws = new WebSocket(target.webSocketDebuggerUrl);
await new Promise(r => (ws.onopen = r));
let id = 0;
const pending = new Map();
ws.onmessage = e => { const m = JSON.parse(e.data); pending.get(m.id)?.(m.result); pending.delete(m.id); };
const send = (method, params = {}) => new Promise(r => { pending.set(++id, r); ws.send(JSON.stringify({ id, method, params })); });

await send('Emulation.setDeviceMetricsOverride', { width: W, height: H, deviceScaleFactor: 2, mobile: false });
await send('Emulation.setDefaultBackgroundColorOverride', { color: { r: 0, g: 0, b: 0, a: 0 } });
await send('Page.navigate', { url: `data:text/html;base64,${Buffer.from(page).toString('base64')}` });
await new Promise(r => setTimeout(r, 500));

// An overlay frame every TICK ms (the recorded frames are 125 ms apart), so a
// cursor glide is smooth; a tick that changes nothing extends the last frame.
const TICK = 62.5;
const entries = [];
let start = 0;
for (const [frame, hold] of frames) {
  for (let t = start; t < start + hold - 1; t += TICK) {
    const ms = Math.round(Math.min(TICK, start + hold - t));
    const state = JSON.stringify(devices.map(d => phone(d, t)));
    const last = entries.at(-1);
    if (last?.frame === frame && last.state === state) last.ms += ms;
    else entries.push({ frame, state, ms });
  }
  start += hold;
}
const overlay = [];
let n = 0;
let shot = null;
let drawn = null;
for (const { frame, state, ms } of entries) {
  if (state !== drawn) {
    await send('Runtime.evaluate', { expression: `draw(${JSON.stringify(devices)}, ${state})` });
    const { data } = await send('Page.captureScreenshot', { format: 'png' });
    shot = join(out, `sim_${String(n++).padStart(3, '0')}.png`);
    writeFileSync(shot, Buffer.from(data, 'base64'));
    drawn = state;
  }
  overlay.push(`${frame} ${shot} ${ms}`);
}
writeFileSync(join(out, 'overlay.txt'), overlay.join('\n') + '\n');
ws.close();
const exited = new Promise(r => chrome.once('exit', r));
chrome.kill();
await exited;
rmSync(profile, { recursive: true, force: true });
