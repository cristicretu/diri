// Draws the Simulator window site-clips.sh lays over the computer-use clip:
// the app Claude Code is testing, with its cursor moving, clicking and typing
// in step with the tool calls in the pane.
//
//   node scripts/site-clip-simulator.mjs <recorded clip dir> <out dir>
//
// Reads the clip's frames.txt (frame, hold) and writes <out>/overlay.txt with
// one line per overlay frame: the recorded frame under it, a transparent PNG
// the size of the cropped clip, and how long it holds. Needs Google Chrome
// (CHROME=<path> to use another build).
import { spawn } from 'node:child_process';
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const [clip, out] = process.argv.slice(2);
// The cropped clip in CSS pixels; Chrome renders at 2x like the recording.
const W = 878, H = 570;

const page = `<!doctype html><meta charset="utf-8"><style>
* { box-sizing: border-box; margin: 0; }
html, body { width: ${W}px; height: ${H}px; background: transparent; overflow: hidden; font-family: -apple-system, "SF Pro Text", sans-serif; -webkit-font-smoothing: antialiased; }
#stage { position: absolute; left: 660px; top: 30px; scale: .93; transform-origin: 0 0; }
#win { position: absolute; left: 0; top: 0; width: 220px; height: 468px; border-radius: 12px; background: #2a2838; border: 1px solid #ffffff1f; box-shadow: 0 18px 50px #00000099, 0 2px 8px #00000066; transform-origin: 50% 40%; }
.bar { height: 26px; display: flex; align-items: center; gap: 6px; padding: 0 10px; }
.bar i { width: 9px; height: 9px; border-radius: 50%; }
.bar span { margin-left: 8px; font-size: 9.5px; font-weight: 600; color: #d8d3e4; }
.phone { position: absolute; left: 6px; top: 26px; width: 208px; height: 436px; border-radius: 30px; background: #0b0b0f; padding: 6px; }
#screen { position: relative; width: 196px; height: 424px; border-radius: 25px; overflow: hidden; background: #f6f6f9; color: #15131c; }
.abs { position: absolute; left: 0; right: 0; text-align: center; }
.island { position: absolute; top: 9px; left: 70px; width: 56px; height: 16px; border-radius: 9px; background: #000; }
.time { position: absolute; top: 10px; left: 22px; font-size: 9.5px; font-weight: 600; }
.logo { position: absolute; top: 92px; left: 76px; width: 44px; height: 44px; border-radius: 12px; background: linear-gradient(135deg, #6d5dfc, #c06cf2); }
h1 { font-size: 16px; font-weight: 650; letter-spacing: -.01em; }
.sub { font-size: 9.5px; color: #6e6a7a; }
#field { position: absolute; top: 208px; left: 18px; width: 160px; height: 30px; border-radius: 9px; border: 1px solid #d9d7e1; background: #fff; font-size: 10.5px; padding: 0 10px; display: flex; align-items: center; color: #9a96a6; }
#field.focus { border-color: #6d5dfc; box-shadow: 0 0 0 3px #6d5dfc26; color: #15131c; }
#field.focus::after { content: ''; width: 1px; height: 13px; margin-left: 1px; background: #6d5dfc; }
#button { position: absolute; top: 248px; left: 18px; width: 160px; height: 30px; border-radius: 9px; background: #6d5dfc; color: #fff; font-size: 10.5px; font-weight: 600; display: flex; align-items: center; justify-content: center; }
#button.pressed { background: #5847e0; transform: scale(.97); }
.art { position: absolute; top: 74px; left: 40px; width: 116px; height: 116px; border-radius: 50%; background: radial-gradient(circle at 35% 30%, #ffffffb3, #fff0 40%), linear-gradient(135deg, #8b7dff, #f08bc8); }
.next { position: absolute; top: 310px; left: 18px; width: 160px; height: 30px; border-radius: 9px; background: #15131c; color: #fff; font-size: 10.5px; font-weight: 600; display: flex; align-items: center; justify-content: center; }
.dots { position: absolute; left: 0; right: 0; display: flex; justify-content: center; gap: 5px; }
.dots i { width: 6px; height: 6px; border-radius: 50%; background: #c9c6d3; }
.dots i:first-child { width: 14px; border-radius: 3px; background: #6d5dfc; }
.skip { font-size: 10px; font-weight: 500; color: #6e6a7a; }
#ring { position: absolute; left: 50px; width: 96px; border-radius: 10px; border: 2px solid; }
#flash { position: absolute; inset: 0; background: #fff; pointer-events: none; }
#cursor { position: absolute; left: 0; top: 0; }
#cursor b { position: absolute; left: 13px; top: 17px; padding: 2px 6px; border-radius: 6px; background: #d97757; color: #fff; font-size: 9px; font-weight: 600; white-space: nowrap; box-shadow: 0 2px 6px #0006; }
#ripple { position: absolute; width: 26px; height: 26px; margin: -13px 0 0 -13px; border-radius: 50%; border: 2px solid #d97757; }
</style>
<div id="stage"><div id="win">
  <div class="bar"><i style="background:#ff5f57"></i><i style="background:#febc2e"></i><i style="background:#28c840"></i><span>iPhone 17 Pro</span></div>
  <div class="phone"><div id="screen"></div></div>
</div>
<div id="cursor"><div id="ripple"></div><svg width="16" height="20" viewBox="0 0 16 20"><path d="M1.5 1.5v15l4-3.8 2.6 6 2.6-1.1-2.6-5.9h5.6z" fill="#d97757" stroke="#fff" stroke-width="1.4" stroke-linejoin="round"/></svg><b>Claude Code</b></div></div>
<script>
const signin = s => \`<div class="island"></div><div class="time">9:41</div><div class="logo"></div>
  <div class="abs" style="top:150px"><h1>Welcome to Acme</h1></div>
  <div class="abs sub" style="top:174px">Sign in with your work email.</div>
  <div id="field" class="\${s.focus ? 'focus' : ''}">\${s.typed || (s.focus ? '' : 'Email')}</div>
  <div id="button" class="\${s.pressed ? 'pressed' : ''}">Continue</div>\`;
// The page dots sit on top of Skip until the fix moves them up.
const welcome = s => \`<div class="island"></div><div class="time">9:41</div><div class="art"></div>
  <div class="abs" style="top:214px"><h1>Welcome, Alex</h1></div>
  <div class="abs sub" style="top:240px;line-height:1.45">Your workspace is ready.<br>Here’s a quick tour.</div>
  <div class="next">Next</div>
  <div class="dots" style="top:\${s.fixed ? 356 : 375}px"><i></i><i></i><i></i></div>
  <div class="abs skip" style="top:\${s.fixed ? 378 : 371}px">Skip</div>
  \${s.ring ? \`<div id="ring" style="top:\${s.fixed ? 348 : 363}px;height:\${s.fixed ? 50 : 26}px;border-color:\${s.ring};opacity:\${s.ringOpacity};box-shadow:0 0 0 4px \${s.ring}26"></div>\` : ''}\`;
window.draw = s => {
  const win = document.getElementById('win');
  win.style.opacity = s.show;
  win.style.transform = \`translateY(\${(1 - s.show) * 8}px) scale(\${0.97 + s.show * 0.03})\`;
  document.getElementById('screen').innerHTML = (s.screen === 'signin' ? signin(s) : welcome(s)) + \`<div id="flash" style="opacity:\${s.flash}"></div>\`;
  const cursor = document.getElementById('cursor');
  cursor.style.display = s.cursor ? '' : 'none';
  // Cursor positions are in screen points; the screen starts 12, 32 into the window.
  if (s.cursor) cursor.style.transform = \`translate(\${12 + s.cursor[0]}px, \${32 + s.cursor[1]}px)\`;
  const ripple = document.getElementById('ripple');
  ripple.style.display = s.ripple == null ? 'none' : '';
  if (s.ripple != null) { ripple.style.transform = \`translate(2px, 2px) scale(\${0.4 + s.ripple})\`; ripple.style.opacity = 1 - s.ripple; }
};
</script>`;

// The overlay's story, one entry per recorded frame in order: Building,
// launch_app, click + type_text, click Continue, get_window_state, the fix,
// and the final check. Each step animates, then holds for the rest of the
// recorded frame.
const ease = t => (t < 0.5 ? 2 * t * t : 1 - (-2 * t + 2) ** 2 / 2);
const glide = (to, n = 12, ms = 30) => s => {
  const from = s.cursor;
  return Array.from({ length: n }, (_, i) => {
    const t = ease((i + 1) / n);
    return [{ cursor: [from[0] + (to[0] - from[0]) * t, from[1] + (to[1] - from[1]) * t] }, ms];
  });
};
const click = (extra = {}) => () => [0, 0.3, 0.6, 0.9].map((r, i) => [{ ripple: r, ...(i === 0 ? extra : {}) }, 40]).concat([[{ ripple: null }, 0]]);
const story = [
  [],
  [() => [0.2, 0.45, 0.7, 0.9, 1].map(show => [{ show }, 30]).concat([[{ cursor: [150, 330] }, 0]])],
  [glide([70, 223]), click({ focus: true }), () => [...'alex@acme.dev'].map((_, i, all) => [{ typed: all.slice(0, i + 1).join('') }, 45])],
  [glide([98, 263]), click({ pressed: true }), () => [[{ pressed: false }, 0]]],
  [() => [[{ screen: 'welcome', flash: 0.7 }, 40], [{ flash: 0.35 }, 40], [{ flash: 0 }, 0]]],
  [() => [0.35, 0.7, 1].map(ringOpacity => [{ ring: '#ef4444', ringOpacity }, 40])],
  [() => [[{ fixed: true, ring: '#22c55e' }, 0]], glide([112, 292])],
];

const frames = readFileSync(join(clip, 'frames.txt'), 'utf8').trim().split('\n').map(l => l.split(/\s+/));
if (frames.length !== story.length) throw new Error(`expected ${story.length} recorded frames, found ${frames.length}`);

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

let state = { show: 0, screen: 'signin', cursor: null, typed: '', focus: false, pressed: false, ripple: null, flash: 0, ring: null, ringOpacity: 1, fixed: false };
const lines = [];
let n = 0;
const shoot = async () => {
  await send('Runtime.evaluate', { expression: `draw(${JSON.stringify(state)})` });
  const { data } = await send('Page.captureScreenshot', { format: 'png' });
  const png = join(out, `sim_${String(n++).padStart(3, '0')}.png`);
  writeFileSync(png, Buffer.from(data, 'base64'));
  return png;
};
for (const [i, [frame, hold]] of frames.entries()) {
  let used = 0;
  const steps = [];
  for (const step of story[i]) for (const [change, ms] of step(state)) { state = { ...state, ...change }; steps.push([{ ...state }, ms]); }
  if (!steps.length) steps.push([{ ...state }, 0]);
  for (const [s, ms] of steps) used += ms;
  if (used > +hold) throw new Error(`${frame}: the overlay needs ${used} ms but the frame holds ${hold}`);
  steps.at(-1)[1] += hold - used;
  for (const [s, ms] of steps) {
    if (!ms) continue;
    state = s;
    lines.push(`${frame} ${await shoot()} ${ms}`);
  }
}
writeFileSync(join(out, 'overlay.txt'), lines.join('\n') + '\n');
ws.close();
const exited = new Promise(r => chrome.once('exit', r));
chrome.kill();
await exited;
rmSync(profile, { recursive: true, force: true });
