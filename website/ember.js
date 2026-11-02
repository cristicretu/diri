// Glyph ember: ASCII fire licking up from a white-hot bed of glyphs, tinted with the hero sky's colors
// (deep iris at the tips, rose and coral in the body, peach going white-hot at the bed).
// A Doom-fire heat field on a monospace grid. Rendering is kept cheap on purpose:
// - every glyph is rasterized once into an atlas per color bucket, so a frame is only drawImage blits;
// - the bloom is the heat field at one pixel per cell, box-blurred in JS on that tiny grid and stretched
//   by the compositor, so no CSS filter re-runs over the full canvas each frame;
// - the top fade is per-row alpha, not a CSS mask (which would force an offscreen pass per frame).
// Paused offscreen and in hidden tabs; one still frame under reduced motion.
const canvas = document.querySelector('.ember-glyphs');
const hazeCanvas = document.querySelector('.ember-haze');
const ctx = canvas?.getContext('2d', { alpha: true });
const haze = hazeCanvas?.getContext('2d', { alpha: true });

if (ctx && haze) {
  const RAMP = " .'`,:;-~^\"!il|/()[]{}?+<>1tfjrxnuvczJLYXUCOZQ0mwqpdbkhao*#MW&8%B@$";
  const SPARKS = ['+', '*', '+', '.'];
  // Heat → color, sampled from the sky palette: iris → rose → coral → peach → white-hot.
  const STOPS = [
    [0.00, [125, 109, 182]], [0.22, [150, 128, 205]], [0.42, [183, 127, 168]],
    [0.60, [220, 147, 135]], [0.80, [246, 205, 178]], [1.00, [255, 246, 238]],
  ];
  const BUCKETS = 12, MIN_HEAT = 0.07, FPS = 30;
  const colorAt = h => {
    let j = 0; while (j < STOPS.length - 2 && h > STOPS[j + 1][0]) j++;
    const [h0, c0] = STOPS[j], [h1, c1] = STOPS[j + 1];
    const k = Math.min(1, Math.max(0, (h - h0) / (h1 - h0)));
    return c0.map((v, i) => Math.round(v + (c1[i] - v) * k));
  };
  const bucketRgb = Array.from({ length: BUCKETS }, (_, b) => colorAt((b + 0.5) / BUCKETS));
  // 256-entry lookup for the haze so the per-cell loop does no color math.
  const hazeLut = Array.from({ length: 256 }, (_, i) => colorAt(i / 255 * 0.82));

  const reduced = matchMedia('(prefers-reduced-motion: reduce)');
  let cols = 0, rows = 0, cw = 0, ch = 0, dpr = 1, gw = 0, gh = 0;
  let hx0 = 0, hx1 = -1, hy0 = 0, hy1 = -1;
  let fuel, heat, next, strength, glyph, blurA, blurB, rowAlpha, hazeImage, atlas, sparks = [], t = 0;

  // Rasterize every ramp glyph in every bucket color, plus glowing spark sprites in the last row.
  const buildAtlas = size => {
    atlas = document.createElement('canvas');
    gw = Math.ceil(cw * dpr); gh = Math.ceil(ch * dpr);
    atlas.width = gw * RAMP.length; atlas.height = gh * (BUCKETS + 1);
    const a = atlas.getContext('2d');
    a.font = `600 ${size * dpr}px ui-monospace, "SF Mono", Menlo, monospace`;
    a.textBaseline = 'top';
    for (let b = 0; b < BUCKETS; b++) {
      const [r, g, bl] = bucketRgb[b], h = (b + 0.5) / BUCKETS;
      a.fillStyle = `rgba(${r},${g},${bl},${Math.min(1, 0.05 + 1.25 * h ** 1.35).toFixed(3)})`;
      for (let k = 1; k < RAMP.length; k++) a.fillText(RAMP[k], k * gw, b * gh);
    }
    a.fillStyle = '#fff3ea'; a.shadowColor = 'rgba(246,205,178,0.95)'; a.shadowBlur = 4 * dpr;
    SPARKS.forEach((s, i) => a.fillText(s, i * gw, BUCKETS * gh));
  };

  // Cells fully behind the hero window are never seen (its glass sidebar blurs them away), so they are
  // not drawn: that is most of the bed and most of the per-frame blits.
  const hero = document.querySelector('.hero-clip');
  const occlude = () => {
    hx1 = hy1 = -1;
    if (!hero || !cols) return;
    const c = canvas.getBoundingClientRect(), h = hero.getBoundingClientRect();
    hx0 = Math.ceil((h.left - c.left) / c.width * cols); hx1 = Math.floor((h.right - c.left) / c.width * cols) - 1;
    hy0 = Math.ceil((h.top - c.top) / c.height * rows); hy1 = Math.floor((h.bottom - c.top) / c.height * rows) - 1;
    // The fire burns under the window: full fuel across its width, easing to nothing a little past each edge,
    // so the sides of the page stay dark instead of ending in a hard-cut band.
    const reach = Math.max(10, cols * 0.2);
    for (let x = 0; x < cols; x++) {
      const out = Math.min(1, Math.max(0, hx0 - x + reach * 0.2, x - hx1 + reach * 0.2) / reach);
      fuel[x] = out >= 1 ? 0 : 1 - out * out * (3 - 2 * out);
    }
  };

  const resize = () => {
    const rect = canvas.getBoundingClientRect();
    if (!rect.width || !rect.height) return false;
    const nextDpr = Math.min(devicePixelRatio || 1, 2);
    if (heat && Math.round(rect.width * nextDpr) === canvas.width && Math.round(rect.height * nextDpr) === canvas.height) return false;
    dpr = nextDpr;
    canvas.width = Math.round(rect.width * dpr);
    canvas.height = Math.round(rect.height * dpr);
    const size = rect.width < 700 ? 9 : 11;
    ctx.font = `600 ${size}px ui-monospace, "SF Mono", Menlo, monospace`;
    cw = Math.ceil(ctx.measureText('M').width); ch = Math.round(size * 1.18);
    cols = Math.ceil(rect.width / cw); rows = Math.ceil(rect.height / ch);
    const n = cols * rows;
    heat = new Float32Array(n); next = new Float32Array(n); strength = new Float32Array(cols);
    fuel = new Float32Array(cols).fill(1);
    blurA = new Float32Array(n); blurB = new Float32Array(n);
    glyph = new Uint8Array(n).map(() => Math.random() * 255);
    // Eased fades: tips dissolve over the top 60%, and the bed softens over the last 10% (it sits behind the window).
    const ease = v => { v = Math.min(1, Math.max(0, v)); return v * v * (3 - 2 * v); };
    rowAlpha = Float32Array.from({ length: rows }, (_, y) => {
      const f = (y + 0.5) / rows;
      return ease(f / 0.6) ** 1.5 * ease((1 - f) / 0.1);
    });
    hazeCanvas.width = cols; hazeCanvas.height = rows;
    hazeImage = haze.createImageData(cols, rows);
    buildAtlas(size);
    return true;
  };

  // Slow, smooth noise across columns: separate tongues of flame instead of a flat wall.
  const tongue = x => {
    const a = Math.sin(x * 0.075 + t * 0.55) * 0.5 + Math.sin(x * 0.19 - t * 0.9 + 1.7) * 0.3 + Math.sin(x * 0.031 + t * 0.21 + 4) * 0.45;
    return Math.max(0, Math.min(1, 0.5 + a * 0.75)) ** 1.4;
  };

  const step = () => {
    t += 1 / FPS;
    const bed = (rows - 1) * cols;
    for (let x = 0; x < cols; x++) {
      const f = strength[x] = tongue(x) * fuel[x];
      heat[bed + x] = fuel[x] * Math.min(1, 0.5 + f * 0.6 + Math.random() * 0.12);
    }
    const wind = Math.sin(t * 0.4) * 0.6, scale = 40 / rows + 0.4;
    for (let y = 0; y < rows - 1; y++) {
      const below = (y + 1) * cols;
      for (let x = 0; x < cols; x++) {
        const sx = Math.min(cols - 1, Math.max(0, Math.round(x + wind * Math.random() - Math.random() * 1.2 + 0.6)));
        const l = heat[below + Math.max(0, sx - 1)], c = heat[below + sx], r = heat[below + Math.min(cols - 1, sx + 1)];
        // Taller tongues where the bed column is hot: decay shrinks with the column's flame strength.
        const decay = (0.006 + Math.random() * 0.026) * (1.7 - strength[x] * 1.45) * scale;
        next[y * cols + x] = Math.max(0, c * 0.6 + (l + r) * 0.2 - decay);
      }
    }
    next.set(heat.subarray(bed), bed);
    [heat, next] = [next, heat];
    for (let i = 0; i < glyph.length; i++) if (Math.random() < 0.06) glyph[i] = Math.random() * 255;

    // Sparks: a few loose glyphs rising out of the hottest tongues.
    if (sparks.length < cols * 0.18 && Math.random() < 0.5) {
      const x = Math.random() * cols;
      if (strength[Math.floor(x)] > 0.45) sparks.push({ x: x * cw, y: (rows - 4 - Math.random() * rows * 0.3) * ch, vy: 0.6 + Math.random() * 1.1, vx: (Math.random() - 0.5) * 0.4, life: 1, g: Math.floor(Math.random() * SPARKS.length) });
    }
    sparks = sparks.filter(s => { s.y -= s.vy; s.x += s.vx + Math.sin((s.y + t * 40) * 0.03) * 0.25; s.life -= 0.006 + Math.random() * 0.004; return s.life > 0 && s.y > -ch; });
  };

  // Bloom: separable 5-tap box blur of the heat on the cell grid, then one pixel per cell.
  const drawHaze = () => {
    for (let y = 0; y < rows; y++) {
      const o = y * cols;
      for (let x = 0; x < cols; x++) {
        let s = 0;
        for (let d = -2; d <= 2; d++) s += heat[o + Math.min(cols - 1, Math.max(0, x + d))];
        blurA[o + x] = s / 5;
      }
    }
    for (let y = 0; y < rows; y++) for (let x = 0; x < cols; x++) {
      let s = 0;
      for (let d = -2; d <= 2; d++) s += blurA[Math.min(rows - 1, Math.max(0, y + d)) * cols + x];
      blurB[y * cols + x] = s / 5;
    }
    const px = hazeImage.data;
    for (let y = 0; y < rows; y++) {
      const fade = rowAlpha[y];
      for (let x = 0; x < cols; x++) {
        const i = y * cols + x, h = Math.min(1, blurB[i]), o = i * 4, rgb = hazeLut[(h * 255) | 0];
        px[o] = rgb[0]; px[o + 1] = rgb[1]; px[o + 2] = rgb[2];
        px[o + 3] = h < 0.05 ? 0 : Math.min(150, h * h * 190) * fade;
      }
    }
    haze.putImageData(hazeImage, 0, 0);
  };

  const draw = () => {
    drawHaze();
    ctx.setTransform(1, 0, 0, 1, 0, 0);
    ctx.clearRect(0, 0, canvas.width, canvas.height);
    const n = RAMP.length - 1;
    for (let y = 0; y < rows; y++) {
      const fade = rowAlpha[y];
      if (fade <= 0.01) continue;
      ctx.globalAlpha = fade;
      const py = Math.round(y * ch * dpr), hidden = y >= hy0 && y <= hy1;
      for (let x = 0; x < cols; x++) {
        if (hidden && x === hx0) { x = hx1; continue; }
        const i = y * cols + x, h = heat[i];
        if (h < MIN_HEAT) continue;
        // Dense glyphs where it's hot; a little per-cell jitter keeps neighbours from repeating.
        const k = Math.min(n, Math.max(1, Math.round(h * n * 0.92 + (glyph[i] / 255 - 0.5) * 9)));
        const b = Math.min(BUCKETS - 1, (h * BUCKETS) | 0);
        ctx.drawImage(atlas, k * gw, b * gh, gw, gh, Math.round(x * cw * dpr), py, gw, gh);
      }
    }
    for (const s of sparks) {
      ctx.globalAlpha = Math.min(1, s.life * 1.4) * (0.6 + Math.random() * 0.4);
      ctx.drawImage(atlas, s.g * gw, BUCKETS * gh, gw, gh, Math.round(s.x * dpr), Math.round(s.y * dpr), gw, gh);
    }
    ctx.globalAlpha = 1;
  };

  let raf = 0, last = 0, visible = true;
  const frame = now => {
    raf = 0;
    if (!visible || document.hidden || reduced.matches) return;
    if (now - last >= 1000 / FPS - 2) { last = now; const a = performance.now(); step(); const b = performance.now(); draw(); const c = performance.now(); const st = (window.__em ??= { n: 0, s: 0, d: 0, cells: cols * rows }); st.n++; st.s += b - a; st.d += c - b; }
    raf = requestAnimationFrame(frame);
  };
  const start = () => {
    if (reduced.matches) { for (let i = 0; i < rows * 1.5; i++) step(); draw(); return; }
    if (!raf) raf = requestAnimationFrame(frame);
  };
  const reset = () => { const fresh = resize(); occlude(); if (fresh) { for (let i = 0; i < rows; i++) step(); draw(); start(); } };

  new IntersectionObserver(([entry]) => { visible = entry.isIntersecting; if (visible) start(); }).observe(canvas);
  document.addEventListener('visibilitychange', start);
  reduced.addEventListener?.('change', start);
  let resizeTimer = 0;
  const sizes = new ResizeObserver(() => { clearTimeout(resizeTimer); resizeTimer = setTimeout(reset, 120); });
  sizes.observe(canvas); if (hero) sizes.observe(hero);
  reset();
}
