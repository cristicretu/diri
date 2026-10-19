// Draws the Simulators site-clips.sh lays over the computer-use clip: one
// phone per agent, each with that agent's cursor moving, clicking and typing
// at the same time, in step with the transcript in the pane. The screens are
// real Simulator screenshots (site-clip-phones.sh) and the bezels are the
// ones Xcode's Simulator draws (/Library/Developer/DeviceKit/Chrome).
//
//   node scripts/site-clip-simulator.mjs <recorded clip dir> <screenshots dir> <out dir> <width> <height>
//
// Reads the clip's frames.txt (frame, hold) and marks.txt (step, ms from the
// start) and writes <out>/overlay.txt with one line per overlay frame: the
// recorded frame under it, a transparent PNG the size of the clip, and how
// long it holds. Needs Google Chrome (CHROME=<path> to use another build).
import { execFileSync, spawn } from 'node:child_process';
import { copyFileSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

// The clip's size in CSS pixels; Chrome renders at 2x like the recording.
const [clip, shots, out, W, H] = process.argv.slice(2).map((arg, i) => (i < 3 ? arg : Number(arg)));
const lines = file => readFileSync(join(clip, file), 'utf8').trim().split('\n').map(l => l.split(/\s+/));
const frames = lines('frames.txt').map(([frame, hold]) => [frame, Number(hold)]);
const mark = Object.fromEntries(lines('marks.txt').map(([name, at]) => [name, Number(at)]));

// Each phone's story in ms. A phone opens just after its agent's tab is
// selected (the codex, pi and claude marks), so the window shows who opened
// it, and each tests something else: Codex signs in on the SE and finds the
// overlap the transcript reports, flagged when the children report back and
// fixed by the rebuild; Pi pages through the tour in dark mode; Claude Code
// turns on notifications through the system alert.
// Devices in points, as the Simulator draws them: `screen` is the display,
// `chrome` the bezel around it. Taps are in screen points, read off the
// screenshots.
const devices = [
  { kind: 'signin', title: 'iPhone SE', agent: 'Codex', tag: '#e8e6ee', ink: '#17151f', x: W - 549, y: 0,
    open: mark.codex + 300, start: mark.codex + 700, shots: 'se', screen: [375, 667], chrome: 'phone' },
  { kind: 'tour', title: 'iPhone 17 Pro Max', agent: 'Pi', tag: '#7f8cff', ink: '#fff', x: W - 375, y: 0,
    open: mark.pi + 300, start: mark.pi + 700, shots: 'max', screen: [440, 956], chrome: 'phone12', dark: true },
  { kind: 'notify', title: 'iPhone 17 Pro', agent: 'Claude Code', tag: '#d97757', ink: '#fff', x: W - 185, y: 0,
    open: mark.claude + 300, start: mark.claude + 700, shots: 'pro', screen: [402, 874], chrome: 'phone11' },
];
const SCALE = 0.38;
// Side by side at one scale, so the SE is visibly the smallest, standing on
// one line above the pane's prompt.
const FLOOR = 590;
const FIELD = [187, 555], CONTINUE = [187, 617], SE_AWAY = [200, 470];
const NEXT = [220, 811], MAX_AWAY = [250, 700];
const NOTIFY = [201, 753], ALLOW = [275, 518], PRO_AWAY = [200, 640];
const EMAIL = 12;
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
  const toButton = typedAt + EMAIL * PER_KEY + 120;
  const pressAt = toButton + GLIDE;
  const welcomeAt = pressAt + PRESS;
  const keys = t >= typedAt ? Math.min(EMAIL, Math.floor((t - typedAt) / PER_KEY) + 1) : 0;
  const fixed = t >= mark.done;
  return {
    cursor: along([[0, SE_AWAY], [d.start, FIELD], [toButton, CONTINUE], [welcomeAt + 500, SE_AWAY]], t),
    ripple: ripple([d.start + GLIDE, pressAt], t),
    shot: t >= welcomeAt ? `welcome-${fixed ? 'fixed' : 'bug'}` : `signin-${keys}`,
    // The overlap Codex reports, flagged red when the children report back
    // and green once the rebuild moves the dots off Skip.
    ring: t >= mark.wait ? { color: fixed ? '#22c55e' : '#ef4444', box: fixed ? [127, 574, 120, 76] : [127, 617, 120, 30], opacity: clamp((t - mark.wait) / 120) } : null,
  };
}

function tour(d, t) {
  const taps = [0, 1, 2].map(i => d.start + GLIDE + i * TOUR_STEP);
  const page = Math.min(2, taps.filter(at => t >= at + PRESS).length);
  return {
    cursor: along([[0, MAX_AWAY], [d.start, NEXT], [taps[2] + 600, MAX_AWAY]], t),
    ripple: ripple(taps, t),
    shot: `tour-${page}`,
  };
}

function notify(d, t) {
  const tapAt = d.start + GLIDE;
  const alertAt = tapAt + PRESS;
  const allowAt = alertAt + 500 + GLIDE;
  return {
    cursor: along([[0, PRO_AWAY], [d.start, NOTIFY], [alertAt + 500, ALLOW], [allowAt + 700, PRO_AWAY]], t),
    ripple: ripple([tapAt, allowAt], t),
    shot: t >= allowAt + PRESS ? 'notify-on' : t >= alertAt ? 'notify-alert' : 'notify',
  };
}

const stories = { signin, tour, notify };
function phone(d, t) {
  const show = clamp((t - d.open) / 160);
  return { show, ...stories[d.kind](d, t), ...(t < d.open + 200 ? { cursor: null, ripple: null } : {}) };
}

// The Simulator's bezels, rasterised from Xcode's PDFs at the 3x the
// screenshots are taken at. The SE's is drawn from nine pieces around its
// screen, with its home button; the 17s have one composite image.
const assets = mkdtempSync(join(tmpdir(), 'site-clip-sim-'));
const kit = '/Library/Developer/DeviceKit/Chrome';
const raster = (chrome, piece, w, h) => {
  const file = join(assets, `${chrome}-${piece.replace(/ /g, '')}.png`);
  execFileSync('sips', ['-s', 'format', 'png', '-z', String(h * 3), String(w * 3), join(kit, `${chrome}.devicechrome/Contents/Resources/${piece}.pdf`), '--out', file], { stdio: 'ignore' });
  return file.slice(assets.length + 1);
};
const SE = { left: 28, top: 111, corner: [97, 111], home: 67 };
const seW = 375 + 2 * SE.left, seH = 667 + 2 * SE.top;
const se = {
  tl: raster('phone', 'Phone TL', ...SE.corner), tr: raster('phone', 'Phone TR', ...SE.corner),
  bl: raster('phone', 'Phone BL', ...SE.corner), br: raster('phone', 'Phone BR', ...SE.corner),
  top: raster('phone', 'PhoneTop', 1, SE.top), base: raster('phone', 'PhoneBase', 1, SE.top),
  left: raster('phone', 'PhoneLeft', SE.left, 1), right: raster('phone', 'PhoneRight', SE.left, 1),
  home: raster('phone', 'Home BTN', SE.home, SE.home),
};
// The 17s' screens sit 17 points inside their composite.
const composite = { phone11: raster('phone11', 'PhoneComposite', 436, 908), phone12: raster('phone12', 'PhoneComposite', 474, 990) };
for (const d of devices) {
  const names = { signin: [...Array(EMAIL + 1).keys()].map(i => `signin-${i}`).concat('welcome-bug', 'welcome-fixed'), tour: ['tour-0', 'tour-1', 'tour-2'], notify: ['notify', 'notify-alert', 'notify-on'] }[d.kind];
  for (const name of names) copyFileSync(join(shots, `${d.shots}-${name}.png`), join(assets, `${d.shots}-${name}.png`));
}

const bezel = d => {
  if (d.chrome === 'phone') {
    const piece = (src, x, y, w, h) => `<img src="${src}" style="left:${x}px;top:${y}px;width:${w}px;height:${h}px">`;
    const [cw, ch] = SE.corner;
    return { w: seW, h: seH, inset: [SE.left, SE.top], html: [
      piece(se.tl, 0, 0, cw, ch), piece(se.top, cw, 0, seW - 2 * cw, ch), piece(se.tr, seW - cw, 0, cw, ch),
      piece(se.left, 0, ch, SE.left, seH - 2 * ch), piece(se.right, seW - SE.left, ch, SE.left, seH - 2 * ch),
      piece(se.bl, 0, seH - ch, cw, ch), piece(se.base, cw, seH - ch, seW - 2 * cw, ch), piece(se.br, seW - cw, seH - ch, cw, ch),
      piece(se.home, (seW - SE.home) / 2, seH - 90, SE.home, SE.home),
    ].join('') };
  }
  const [w, h] = d.chrome === 'phone11' ? [436, 908] : [474, 990];
  return { w, h, inset: [17, 17], html: `<img src="${composite[d.chrome]}" style="left:0;top:0;width:${w}px;height:${h}px">` };
};
const frames3d = devices.map(bezel);
devices.forEach((d, i) => { d.y = FLOOR - frames3d[i].h * SCALE; });

const page = `<!doctype html><meta charset="utf-8"><style>
* { box-sizing: border-box; margin: 0; }
html, body { width: ${W}px; height: ${H}px; background: transparent; overflow: hidden; font-family: -apple-system, "SF Pro Text", sans-serif; -webkit-font-smoothing: antialiased; }
.stage { position: absolute; scale: ${SCALE}; transform-origin: 0 0; }
.device { position: absolute; left: 0; top: 0; filter: drop-shadow(0 24px 40px #000a) drop-shadow(0 2px 6px #0007); }
.device img { position: absolute; }
.device .screen { position: absolute; }
.title { position: absolute; left: 0; right: 0; top: -44px; text-align: center; font-size: 26px; font-weight: 600; color: #d8d3e4; white-space: nowrap; }
.ring { position: absolute; border-radius: 16px; border: 4px solid; }
.cursor { position: absolute; left: 0; top: 0; }
.cursor svg { width: 32px; height: 40px; }
.cursor b { position: absolute; left: 26px; top: 34px; padding: 4px 12px; border-radius: 12px; font-size: 18px; font-weight: 600; white-space: nowrap; box-shadow: 0 4px 12px #0006; }
.ripple { position: absolute; width: 52px; height: 52px; margin: -26px 0 0 -26px; border-radius: 50%; border: 4px solid; }
</style><body></body>
<script>
const frames = ${JSON.stringify(frames3d)};
window.draw = (devices, states) => {
  document.body.innerHTML = devices.map((d, i) => {
    const s = states[i];
    if (!s.show) return '';
    const f = frames[i];
    const [ix, iy] = f.inset;
    const cursor = s.cursor ? \`<div class="cursor" style="transform:translate(\${ix + s.cursor[0]}px,\${iy + s.cursor[1]}px)">
      \${s.ripple == null ? '' : \`<div class="ripple" style="border-color:\${d.tag};transform:translate(4px,4px) scale(\${0.4 + s.ripple});opacity:\${1 - s.ripple}"></div>\`}
      <svg viewBox="0 0 16 20"><path d="M1.5 1.5v15l4-3.8 2.6 6 2.6-1.1-2.6-5.9h5.6z" fill="\${d.tag}" stroke="\${d.dark ? '#000' : '#fff'}" stroke-width="1.4" stroke-linejoin="round"/></svg>
      <b style="background:\${d.tag};color:\${d.ink}">\${d.agent}</b></div>\` : '';
    const ring = s.ring ? \`<div class="ring" style="left:\${ix + s.ring.box[0]}px;top:\${iy + s.ring.box[1]}px;width:\${s.ring.box[2]}px;height:\${s.ring.box[3]}px;border-color:\${s.ring.color};opacity:\${s.ring.opacity};box-shadow:0 0 0 6px \${s.ring.color}30"></div>\` : '';
    return \`<div class="stage" style="left:\${d.x}px;top:\${d.y}px;width:\${f.w}px;opacity:\${s.show};translate:0 \${(1 - s.show) * 8}px">
      <div class="title">\${d.title}</div>
      <div class="device" style="width:\${f.w}px;height:\${f.h}px">\${f.html}
        <img class="screen" src="\${d.shots}-\${s.shot}.png" style="left:\${ix}px;top:\${iy}px;width:\${d.screen[0]}px;height:\${d.screen[1]}px">
      </div>\${ring}\${cursor}</div>\`;
  }).join('');
};
</script>`;
writeFileSync(join(assets, 'index.html'), page);

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
await send('Page.navigate', { url: `file://${join(assets, 'index.html')}` });
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
    // Wait for the screenshots the frame swaps in to decode.
    await send('Runtime.evaluate', { expression: `draw(${JSON.stringify(devices)}, ${state}); Promise.all([...document.images].map(i => i.decode()))`, awaitPromise: true });
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
rmSync(assets, { recursive: true, force: true });
