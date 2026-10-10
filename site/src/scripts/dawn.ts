// The hero: the app icon, drawn live. Night sky, a sun that rises once when the page
// opens, and the path of light it lays on the water (the "wake" in the icon). The sun
// sits on the horizon marker in the page, so it always rises just under the buttons.

type RGB = [number, number, number];

const hex = (h: string): RGB => [parseInt(h.slice(1, 3), 16), parseInt(h.slice(3, 5), 16), parseInt(h.slice(5, 7), 16)];
const mix = (a: RGB, b: RGB, t: number): string =>
  `rgb(${Math.round(a[0] + (b[0] - a[0]) * t)} ${Math.round(a[1] + (b[1] - a[1]) * t)} ${Math.round(a[2] + (b[2] - a[2]) * t)})`;
const easeOut = (t: number) => 1 - Math.pow(1 - t, 3);
const clamp01 = (t: number) => Math.min(1, Math.max(0, t));

// Sky stops from the zenith down to the horizon: before the sun, and once it is up.
const SKY_STOPS = [0, 0.45, 0.7, 0.84, 0.94, 1];
const NIGHT = ['#060f22', '#0b1a35', '#132849', '#1a2f52', '#24365a', '#2e3d5e'].map(hex);
const DAWN = ['#081429', '#10234a', '#1f3c63', '#55486a', '#c07a62', '#ffb273'].map(hex);
const SEA_TOP: [RGB, RGB] = [hex('#162d50'), hex('#20406b')];
const SEA_BOTTOM = hex('#0b1730');

interface Star {
  x: number;
  y: number;
  r: number;
  phase: number;
}

export function startDawn(canvas: HTMLCanvasElement, hero: HTMLElement, marker: HTMLElement) {
  const ctx = canvas.getContext('2d');
  if (!ctx) return;

  const reduced = window.matchMedia('(prefers-reduced-motion: reduce)').matches;
  const INTRO_MS = 2800;
  let w = 0;
  let h = 0;
  let dpr = 1;
  let horizon = 0;
  let stars: Star[] = [];
  // Where each streak breaks, and the glints on the water: fixed per page view.
  const ROWS = 18;
  let rnd = 11;
  const rand = () => ((rnd = (rnd * 16807) % 2147483647) / 2147483647);
  const rowCuts = Array.from({ length: ROWS }, (_, k) => {
    const n = 1 + Math.floor(rand() * 3) + (k % 3 === 0 ? 1 : 0);
    const cuts = Array.from({ length: n - 1 }, () => 0.15 + rand() * 0.7).sort((a, b) => a - b);
    return [0, ...cuts, 1];
  });
  const widthJitter = Array.from({ length: ROWS }, (_, k) => (k === 0 ? 1 : 0.86 + rand() * 0.24));
  const glints = Array.from({ length: 60 }, () => ({
    u: rand(),
    v: Math.pow(rand(), 1.6),
    speed: 0.6 + rand() * 1.6,
    phase: rand() * Math.PI * 2,
  }));
  let pointer = 0.5;
  let start = performance.now();
  let raf = 0;
  let visible = true;
  let last = 0;

  function measure() {
    dpr = Math.min(window.devicePixelRatio || 1, 2);
    w = hero.clientWidth;
    h = hero.clientHeight;
    canvas.width = Math.round(w * dpr);
    canvas.height = Math.round(h * dpr);
    horizon = marker.getBoundingClientRect().top - hero.getBoundingClientRect().top;
    // Fewer stars on small screens; they sit in the upper sky only.
    const count = Math.round(Math.min(90, (w * horizon) / 9000));
    let seed = 7;
    const rand = () => ((seed = (seed * 16807) % 2147483647) / 2147483647);
    stars = Array.from({ length: count }, () => ({
      x: rand() * w,
      y: rand() * horizon * 0.72,
      r: 0.5 + rand() * 1.1,
      phase: rand() * Math.PI * 2,
    }));
  }

  function draw(now: number) {
    const elapsed = reduced ? INTRO_MS : now - start;
    const p = easeOut(clamp01(elapsed / INTRO_MS));
    const t = now / 1000;
    const c = ctx!;
    c.setTransform(dpr, 0, 0, dpr, 0, 0);

    // Sky
    const sky = c.createLinearGradient(0, 0, 0, horizon);
    SKY_STOPS.forEach((s, i) => sky.addColorStop(s, mix(NIGHT[i], DAWN[i], p)));
    c.fillStyle = sky;
    c.fillRect(0, 0, w, horizon);

    // Stars fade as the sky brightens
    for (const s of stars) {
      const tw = 0.55 + 0.45 * Math.sin(t * 1.3 + s.phase);
      const a = tw * (1 - p * 0.75) * (1 - s.y / (horizon * 0.8)) * 0.9;
      if (a <= 0.02) continue;
      c.fillStyle = `rgb(232 240 255 / ${a})`;
      c.beginPath();
      c.arc(s.x, s.y, s.r, 0, Math.PI * 2);
      c.fill();
    }

    const r = Math.max(52, Math.min(w * 0.07, 96));
    const sx = w / 2 + (pointer - 0.5) * Math.min(w * 0.04, 48);
    const sy = horizon + r * 1.15 * (1 - p);

    // Sun, its halos and the wide glow, clipped to the sky like the icon
    c.save();
    c.beginPath();
    c.rect(0, 0, w, horizon);
    c.clip();
    const glow = c.createRadialGradient(sx, horizon, 0, sx, horizon, Math.max(w * 0.55, r * 7));
    glow.addColorStop(0, `rgb(255 178 115 / ${0.38 * p})`);
    glow.addColorStop(0.35, `rgb(232 147 95 / ${0.14 * p})`);
    glow.addColorStop(1, 'rgb(232 147 95 / 0)');
    c.fillStyle = glow;
    c.fillRect(0, 0, w, horizon);
    c.fillStyle = 'rgb(255 174 110 / 0.2)';
    c.beginPath();
    c.arc(sx, sy, r * 2, 0, Math.PI * 2);
    c.fill();
    c.fillStyle = 'rgb(255 194 134 / 0.22)';
    c.beginPath();
    c.arc(sx, sy, r * 1.43, 0, Math.PI * 2);
    c.fill();
    const sun = c.createRadialGradient(sx, sy, 0, sx, sy, r);
    sun.addColorStop(0, '#fff0d0');
    sun.addColorStop(1, '#ffa95e');
    c.fillStyle = sun;
    c.beginPath();
    c.arc(sx, sy, r, 0, Math.PI * 2);
    c.fill();
    c.restore();

    // Sea
    const sea = c.createLinearGradient(0, horizon, 0, h);
    sea.addColorStop(0, mix(SEA_TOP[0], SEA_TOP[1], p));
    sea.addColorStop(Math.min(1, 520 / Math.max(1, h - horizon)), mix(SEA_BOTTOM, SEA_BOTTOM, 0));
    sea.addColorStop(1, mix(SEA_BOTTOM, SEA_BOTTOM, 0));
    c.fillStyle = sea;
    c.fillRect(0, horizon, w, h - horizon);

    // The horizon line catches the light
    const line = c.createLinearGradient(sx - w * 0.5, 0, sx + w * 0.5, 0);
    line.addColorStop(0, 'rgb(255 178 115 / 0)');
    line.addColorStop(0.5, `rgb(255 200 150 / ${0.75 * p})`);
    line.addColorStop(1, 'rgb(255 178 115 / 0)');
    c.fillStyle = line;
    c.fillRect(0, horizon - 0.5, w, 1.5);

    // The path of light: streaks that shorten and fade toward the viewer, each one
    // brightest under the sun and dissolving at its ends, broken where the water
    // moves; glints come and go on top. The column follows the icon's proportions.
    c.save();
    c.lineCap = 'round';
    for (let k = 0; k < ROWS; k++) {
      const depth = k / (ROWS - 1);
      const y = horizon + r * (0.13 + k * 0.15 + k * k * 0.014);
      if (y > h) break;
      const sway = reduced ? 0 : Math.sin(t * 0.7 + k * 0.9) * r * 0.05;
      const cx = sx + sway;
      const width = r * 3.1 * Math.pow(1 - depth * 0.78, 1.05) * widthJitter[k];
      const thick = Math.max(1.2, r * (0.04 + depth * 0.05) * (1 - depth * 0.6) * 1.6);
      const alpha = (1 - depth * 0.85) * p;
      const fade = c.createLinearGradient(cx - width / 2, 0, cx + width / 2, 0);
      fade.addColorStop(0, 'rgb(255 179 126 / 0)');
      fade.addColorStop(0.2, `rgb(255 179 126 / ${alpha * 0.7})`);
      fade.addColorStop(0.5, `rgb(255 222 182 / ${alpha})`);
      fade.addColorStop(0.8, `rgb(255 179 126 / ${alpha * 0.7})`);
      fade.addColorStop(1, 'rgb(255 179 126 / 0)');
      c.strokeStyle = fade;
      const cuts = rowCuts[k];
      // Two passes: a wide, faint bloom, then the bright core.
      for (const [lw, a] of [
        [thick * 3.2, 0.22],
        [thick, 1],
      ] as const) {
        c.lineWidth = lw;
        for (let i = 0; i < cuts.length - 1; i++) {
          const drift = reduced ? 0.5 : 0.5 + 0.5 * Math.sin(t * (0.8 + k * 0.05) + k * 2.1 + i * 1.9);
          const gap = width * (0.01 + 0.045 * drift);
          const a0 = cx - width / 2 + width * cuts[i] + (i === 0 ? 0 : gap / 2);
          const a1 = cx - width / 2 + width * cuts[i + 1] - (i === cuts.length - 2 ? 0 : gap / 2);
          if (a1 - a0 < thick) continue;
          const flicker = reduced ? 1 : 0.72 + 0.28 * Math.sin(t * 2.2 + k * 1.3 + i * 2.9);
          c.globalAlpha = a * flicker;
          c.beginPath();
          c.moveTo(a0, y);
          c.lineTo(a1, y);
          c.stroke();
        }
      }
    }
    c.globalAlpha = 1;
    c.globalCompositeOperation = 'lighter';
    for (const g of glints) {
      const y = horizon + g.v * r * 3.4;
      if (y > h) continue;
      const spread = r * 3.1 * (1 - g.v * 0.72);
      const x = sx + (g.u - 0.5) * spread;
      const on = reduced ? 0 : Math.max(0, Math.sin(t * g.speed + g.phase));
      const a = on * on * on * (1 - g.v * 0.8) * p;
      if (a < 0.04) continue;
      const size = 1 + (1 - g.v) * 1.6;
      const spark = c.createRadialGradient(x, y, 0, x, y, size * 3);
      spark.addColorStop(0, `rgb(255 244 225 / ${a})`);
      spark.addColorStop(1, 'rgb(255 200 150 / 0)');
      c.fillStyle = spark;
      c.fillRect(x - size * 3, y - size * 3, size * 6, size * 6);
    }
    c.restore();
  }

  function frame(now: number) {
    raf = 0;
    if (!visible) return;
    // Full rate while the sun rises, then a calm 30 fps for the shimmer.
    if (now - start > INTRO_MS && now - last < 32) {
      raf = requestAnimationFrame(frame);
      return;
    }
    last = now;
    draw(now);
    if (!reduced) raf = requestAnimationFrame(frame);
  }

  function kick() {
    if (!raf) raf = requestAnimationFrame(frame);
  }

  measure();
  start = performance.now();
  draw(start);
  kick();

  new ResizeObserver(() => {
    measure();
    draw(performance.now());
  }).observe(hero);

  new IntersectionObserver(([entry]) => {
    visible = entry.isIntersecting && !document.hidden;
    if (visible) kick();
  }).observe(hero);

  document.addEventListener('visibilitychange', () => {
    visible = !document.hidden;
    if (visible) kick();
  });

  if (!reduced) {
    window.addEventListener(
      'pointermove',
      (e) => {
        pointer = e.clientX / Math.max(1, window.innerWidth);
      },
      { passive: true },
    );
  }
}
