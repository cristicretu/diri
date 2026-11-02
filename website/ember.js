// Glyph ember: ASCII fire licking up from a white-hot bed of glyphs, tinted with the hero sky's own colors
// (deep iris at the tips, rose and coral in the body, the peach horizon going white-hot at the bed).
// A Doom-fire heat field on a monospace grid, drawn in a handful of color buckets so each frame is one
// fillStyle change per bucket. Paused offscreen and in hidden tabs; one still frame under reduced motion.
const canvas = document.querySelector('.ember-glyphs');
const hazeCanvas = document.querySelector('.ember-haze');
const ctx = canvas?.getContext('2d', { alpha: true });
const haze = hazeCanvas?.getContext('2d', { alpha: true });

if (ctx && haze) {
  const RAMP = " .'`,:;-~^\"!il|/()[]{}?+<>1tfjrxnuvczJLYXUCOZQ0mwqpdbkhao*#MW&8%B@$";
  const SPARK_GLYPHS = ['+', '*', '·', '+', '.'];
  // Heat → color, sampled from .page-sky: #6e5d96 / #7d6db6 → #b77fa8 → #dc9387 → #f6cdb2 → white-hot.
  const STOPS = [
    [0.00, [125, 109, 182]], [0.22, [150, 128, 205]], [0.42, [183, 127, 168]],
    [0.60, [220, 147, 135]], [0.80, [246, 205, 178]], [1.00, [255, 246, 238]],
  ];
  const BUCKETS = 14;
  const hazeRgb = [];
  const lerp = (a, b, t) => a + (b - a) * t;
  const palette = Array.from({ length: BUCKETS }, (_, i) => {
    const h = (i + 0.5) / BUCKETS;
    let j = 0; while (j < STOPS.length - 2 && h > STOPS[j + 1][0]) j++;
    const [h0, c0] = STOPS[j], [h1, c1] = STOPS[j + 1];
    const t = Math.min(1, Math.max(0, (h - h0) / (h1 - h0)));
    const rgb = c0.map((v, k) => Math.round(lerp(v, c1[k], t)));
    hazeRgb.push(rgb);
    const alpha = Math.min(1, 0.3 + h * 1.1);
    return `rgba(${rgb[0]},${rgb[1]},${rgb[2]},${alpha.toFixed(3)})`;
  });

  const reduced = matchMedia('(prefers-reduced-motion: reduce)');
  let cols = 0, rows = 0, cw = 0, ch = 0, font = '', dpr = 1;
  let heat, next, glyph, hazeImage, sparks = [], t = 0;

  const resize = () => {
    const rect = canvas.getBoundingClientRect();
    if (!rect.width || !rect.height) return false;
    if (heat && Math.round(rect.width * Math.min(devicePixelRatio || 1, 2)) === canvas.width && Math.round(rect.height * Math.min(devicePixelRatio || 1, 2)) === canvas.height) return false;
    dpr = Math.min(devicePixelRatio || 1, 2);
    canvas.width = Math.round(rect.width * dpr);
    canvas.height = Math.round(rect.height * dpr);
    const size = rect.width < 700 ? 9 : 11;
    font = `600 ${size}px ui-monospace, "SF Mono", Menlo, monospace`;
    ctx.font = font;
    cw = Math.ceil(ctx.measureText('M').width); ch = Math.round(size * 1.18);
    cols = Math.ceil(rect.width / cw); rows = Math.ceil(rect.height / ch);
    heat = new Float32Array(cols * rows); next = new Float32Array(cols * rows);
    glyph = new Uint8Array(cols * rows).map(() => Math.random() * 255);
    hazeCanvas.width = cols; hazeCanvas.height = rows;
    hazeImage = haze.createImageData(cols, rows);
    return true;
  };

  // Slow, smooth noise across columns: separate tongues of flame instead of a flat wall.
  const tongue = x => {
    const a = Math.sin(x * 0.075 + t * 0.55) * 0.5 + Math.sin(x * 0.19 - t * 0.9 + 1.7) * 0.3 + Math.sin(x * 0.031 + t * 0.21 + 4) * 0.45;
    return Math.max(0, Math.min(1, 0.5 + a * 0.75)) ** 1.4;
  };

  const step = () => {
    t += 1 / 30;
    const bed = (rows - 1) * cols;
    for (let x = 0; x < cols; x++) {
      const f = tongue(x);
      heat[bed + x] = Math.min(1, 0.5 + f * 0.6 + Math.random() * 0.12);
      heat[bed - cols + x] = Math.max(heat[bed - cols + x], heat[bed + x] * (0.75 + Math.random() * 0.2));
    }
    const wind = Math.sin(t * 0.4) * 0.6;
    for (let y = 0; y < rows - 1; y++) {
      for (let x = 0; x < cols; x++) {
        const below = (y + 1) * cols;
        const drift = Math.round(x + wind * Math.random() - Math.random() * 1.2 + 0.6);
        const sx = Math.min(cols - 1, Math.max(0, drift));
        const l = heat[below + Math.max(0, sx - 1)], c = heat[below + sx], r = heat[below + Math.min(cols - 1, sx + 1)];
        // Taller tongues where the bed column is hot: decay shrinks with the column's flame strength.
        const decay = (0.006 + Math.random() * 0.026) * (1.7 - tongue(x) * 1.45) * (40 / rows + 0.4);
        next[y * cols + x] = Math.max(0, (c * 0.6 + (l + r) * 0.2) - decay);
      }
    }
    next.set(heat.subarray(bed), bed);
    [heat, next] = [next, heat];
    for (let i = 0; i < glyph.length; i++) if (Math.random() < 0.06) glyph[i] = Math.random() * 255;

    // Sparks: a few loose glyphs rising out of the hottest tongues.
    if (sparks.length < cols * 0.18 && Math.random() < 0.5) {
      const x = Math.random() * cols;
      if (tongue(Math.floor(x)) > 0.45) sparks.push({ x: x * cw, y: (rows - 4 - Math.random() * rows * 0.3) * ch, vy: 0.6 + Math.random() * 1.1, vx: (Math.random() - 0.5) * 0.4, life: 1, g: SPARK_GLYPHS[Math.floor(Math.random() * SPARK_GLYPHS.length)] });
    }
    sparks = sparks.filter(s => { s.y -= s.vy; s.x += s.vx + Math.sin((s.y + t * 40) * 0.03) * 0.25; s.life -= 0.006 + Math.random() * 0.004; return s.life > 0 && s.y > -ch; });
  };

  // One pixel per cell: color by heat, alpha rising steeply so only the body and bed glow.
  const drawHaze = () => {
    const px = hazeImage.data;
    for (let i = 0; i < heat.length; i++) {
      const h = heat[i], rgb = hazeRgb[Math.min(BUCKETS - 1, Math.floor(h * BUCKETS))], o = i * 4;
      px[o] = rgb[0]; px[o + 1] = rgb[1]; px[o + 2] = rgb[2]; px[o + 3] = h < 0.05 ? 0 : Math.min(255, Math.sqrt(h) * h * 330);
    }
    haze.putImageData(hazeImage, 0, 0);
  };

  const draw = () => {
    drawHaze();
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.clearRect(0, 0, canvas.width, canvas.height);
    ctx.font = font; ctx.textBaseline = 'top';
    ctx.globalCompositeOperation = 'lighter';
    const n = RAMP.length - 1;
    for (let b = 0; b < BUCKETS; b++) {
      const lo = b / BUCKETS, hi = (b + 1) / BUCKETS;
      if (hi < 0.07) continue;
      ctx.fillStyle = palette[b];
      for (let y = 0; y < rows; y++) for (let x = 0; x < cols; x++) {
        const h = heat[y * cols + x];
        if (h < lo || h >= hi || h < 0.07) continue;
        // Dense glyphs where it's hot; a little per-cell jitter keeps neighbours from repeating.
        const k = Math.min(n, Math.max(1, Math.round(h * n * 0.92 + (glyph[y * cols + x] / 255 - 0.5) * 9)));
        ctx.fillText(RAMP[k], x * cw, y * ch);
      }
    }
    ctx.shadowColor = 'rgba(246,205,178,0.9)';
    for (const s of sparks) {
      ctx.globalAlpha = Math.min(1, s.life * 1.4) * (0.6 + Math.random() * 0.4);
      ctx.shadowBlur = 8;
      ctx.fillStyle = '#fff3ea';
      ctx.fillText(s.g, s.x, s.y);
    }
    ctx.globalAlpha = 1; ctx.shadowBlur = 0; ctx.globalCompositeOperation = 'source-over';
  };

  let raf = 0, last = 0, visible = true;
  const frame = now => {
    raf = 0;
    if (!visible || document.hidden || reduced.matches) return;
    if (now - last >= 1000 / 30) { last = now; step(); draw(); }
    raf = requestAnimationFrame(frame);
  };
  const start = () => {
    if (reduced.matches) { for (let i = 0; i < rows * 1.5; i++) step(); draw(); return; }
    if (!raf) raf = requestAnimationFrame(frame);
  };
  const reset = () => { if (resize()) { for (let i = 0; i < rows; i++) step(); draw(); start(); } };

  new IntersectionObserver(([entry]) => { visible = entry.isIntersecting; if (visible) start(); }).observe(canvas);
  document.addEventListener('visibilitychange', start);
  reduced.addEventListener?.('change', start);
  let resizeTimer = 0;
  new ResizeObserver(() => { clearTimeout(resizeTimer); resizeTimer = setTimeout(reset, 120); }).observe(canvas);
  reset();
}
