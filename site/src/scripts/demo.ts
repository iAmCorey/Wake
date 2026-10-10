// The search demo: a Wake window that runs the product's core loop on made-up sessions.
// Left alone it types a search, picks a hit and opens the session at the matching
// message, like ⌘K in the app. Any touch hands it over to the visitor.
import type { DemoSession } from '../lib/demo-data';

interface Labels {
  messages: string;
  none: string;
  units: { m: string; h: string; d: string; w: string };
}

interface Agent {
  name: string;
  icon: string;
}

interface Hit {
  session: DemoSession;
  /** Index of the message that matched, or -1 when only the title did. */
  message: number;
  snippet: string;
}

const esc = (s: string) => s.replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[c]!);

const terms = (q: string) => q.toLowerCase().split(/\s+/).filter(Boolean);

function highlight(text: string, ts: string[]): string {
  if (!ts.length) return esc(text);
  const lower = text.toLowerCase();
  const marks: [number, number][] = [];
  for (const term of ts) {
    let from = 0;
    while ((from = lower.indexOf(term, from)) !== -1) {
      marks.push([from, from + term.length]);
      from += term.length;
    }
  }
  marks.sort((a, b) => a[0] - b[0]);
  let out = '';
  let at = 0;
  for (const [s, e] of marks) {
    if (s < at) continue;
    out += esc(text.slice(at, s)) + '<mark>' + esc(text.slice(s, e)) + '</mark>';
    at = e;
  }
  return out + esc(text.slice(at));
}

function snippet(text: string, ts: string[]): string {
  const lower = text.toLowerCase();
  const first = Math.min(...ts.map((t) => lower.indexOf(t)).filter((i) => i >= 0));
  const from = Math.max(0, first - 28);
  const to = Math.min(text.length, first + 90);
  return (from > 0 ? '…' : '') + highlight(text.slice(from, to), ts) + (to < text.length ? '…' : '');
}

function search(sessions: DemoSession[], q: string): Hit[] {
  const ts = terms(q);
  if (!ts.length) return [];
  const hits: Hit[] = [];
  for (const s of sessions) {
    const hay = [s.title, ...s.messages.map((m) => m.text)].join('\n').toLowerCase();
    if (!ts.every((t) => hay.includes(t))) continue;
    const message = s.messages.findIndex((m) => m.role !== 'tool' && ts.some((t) => m.text.toLowerCase().includes(t)));
    hits.push({
      session: s,
      message,
      snippet: message >= 0 ? snippet(s.messages[message].text, ts) : highlight(s.title, ts),
    });
  }
  return hits.sort((a, b) => a.session.age - b.session.age).slice(0, 5);
}

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

export function startDemo(root: HTMLElement) {
  const data = JSON.parse(root.querySelector('script[type="application/json"]')!.textContent!) as {
    sessions: DemoSession[];
    agents: Record<string, Agent>;
    queries: string[];
    labels: Labels;
  };
  const { sessions, agents, queries, labels } = data;
  const win = root.querySelector<HTMLElement>('.window')!;
  const input = root.querySelector<HTMLInputElement>('[data-input]')!;
  const results = root.querySelector<HTMLElement>('[data-results]')!;
  const layer = root.querySelector<HTMLElement>('[data-layer]')!;
  const list = root.querySelector<HTMLElement>('[data-list]')!;
  const detail = {
    ctx: root.querySelector<HTMLElement>('[data-ctx]')!,
    title: root.querySelector<HTMLElement>('[data-title]')!,
    meta: root.querySelector<HTMLElement>('[data-meta]')!,
    body: root.querySelector<HTMLElement>('[data-body]')!,
    msgs: root.querySelector<HTMLElement>('[data-msgs]')!,
  };
  const reduced = window.matchMedia('(prefers-reduced-motion: reduce)').matches;

  let hits: Hit[] = [];
  let current = 0;
  let auto = !reduced;
  let inView = false;

  const age = (m: number) =>
    m < 60 ? `${m}${labels.units.m}` : m < 1440 ? `${Math.round(m / 60)}${labels.units.h}` : m < 10080 ? `${Math.round(m / 1440)}${labels.units.d}` : `${Math.round(m / 10080)}${labels.units.w}`;
  const icon = (id: string, cls = '') => `<img src="/brands/${agents[id].icon}.webp" alt="" width="16" height="16" class="${cls}">`;

  function renderResults() {
    const ts = terms(input.value);
    if (!ts.length) {
      results.innerHTML = '';
      return;
    }
    if (!hits.length) {
      results.innerHTML = `<div class="palette-empty">${esc(labels.none.replace('{q}', input.value))}</div>`;
      return;
    }
    results.innerHTML = hits
      .map(
        (h, i) => `<button type="button" class="hit${i === current ? ' is-current' : ''}" role="option" aria-selected="${i === current}" data-i="${i}">
          <span class="hit-top">${icon(h.session.agent)}<span class="t">${highlight(h.session.title, ts)}</span><span class="where">${esc(h.session.project)}, ${age(h.session.age)}</span></span>
          <span class="hit-snip">${h.snippet}</span>
        </button>`,
      )
      .join('');
  }

  function setQuery(q: string) {
    input.value = q;
    hits = search(sessions, q);
    current = 0;
    renderResults();
  }

  function openPalette() {
    win.classList.add('is-searching');
  }

  function closePalette() {
    win.classList.remove('is-searching');
  }

  function openSession(id: string, ts: string[] = [], hitIndex = -1) {
    const s = sessions.find((x) => x.id === id);
    if (!s) return;
    const a = agents[s.agent];
    detail.ctx.innerHTML = `${icon(s.agent)}<span>${esc(a.name)}</span><span class="proj">${esc(s.project)}</span>${s.branch ? `<span>⑂ ${esc(s.branch)}</span>` : ''}`;
    detail.title.textContent = s.title;
    detail.meta.innerHTML = `${s.model ? `<span class="model-badge">${esc(s.model)}</span>` : ''}<span>${s.messages.length} ${esc(labels.messages)}</span><span>${age(s.age)}</span>`;
    detail.msgs.innerHTML = s.messages
      .map((m, i) => {
        const hit = i === hitIndex;
        const body = hit ? highlight(m.text, ts) : esc(m.text);
        return `<div class="msg msg-${m.role}${hit ? ' is-hit' : ''}">${body}</div>`;
      })
      .join('');
    detail.msgs.style.transform = '';
    list.querySelectorAll('.list-row').forEach((row) => row.classList.toggle('is-current', (row as HTMLElement).dataset.id === id));
    const hitEl = detail.msgs.querySelector<HTMLElement>('.is-hit');
    if (hitEl) {
      requestAnimationFrame(() => {
        const room = detail.body.clientHeight;
        const bottom = hitEl.offsetTop + hitEl.offsetHeight;
        if (bottom > room - 24) detail.msgs.style.transform = `translateY(${-(hitEl.offsetTop - room * 0.25)}px)`;
      });
    }
  }

  function openHit(i: number) {
    const h = hits[i];
    if (!h) return;
    closePalette();
    openSession(h.session.id, terms(input.value), h.message);
  }

  function takeOver() {
    auto = false;
  }

  // The visitor's controls
  root.querySelector('[data-open]')?.addEventListener('click', () => {
    takeOver();
    openPalette();
    input.focus();
    input.select();
  });
  input.addEventListener('input', () => {
    takeOver();
    hits = search(sessions, input.value);
    current = 0;
    renderResults();
  });
  input.addEventListener('keydown', (e) => {
    takeOver();
    if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
      e.preventDefault();
      if (!hits.length) return;
      current = (current + (e.key === 'ArrowDown' ? 1 : hits.length - 1)) % hits.length;
      renderResults();
    } else if (e.key === 'Enter') {
      e.preventDefault();
      openHit(current);
    } else if (e.key === 'Escape') {
      closePalette();
    }
  });
  results.addEventListener('click', (e) => {
    const btn = (e.target as HTMLElement).closest<HTMLElement>('.hit');
    if (!btn) return;
    takeOver();
    openHit(Number(btn.dataset.i));
  });
  layer.addEventListener('pointerdown', (e) => {
    if (e.target === layer) {
      takeOver();
      closePalette();
    }
  });
  list.addEventListener('click', (e) => {
    const row = (e.target as HTMLElement).closest<HTMLElement>('.list-row');
    if (!row) return;
    takeOver();
    closePalette();
    openSession(row.dataset.id!);
  });
  win.addEventListener('pointerdown', takeOver);
  root.querySelectorAll<HTMLButtonElement>('[data-q]').forEach((btn) =>
    btn.addEventListener('click', () => {
      takeOver();
      openPalette();
      setQuery(btn.dataset.q!);
      input.focus();
    }),
  );
  document.addEventListener('keydown', (e) => {
    if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === 'k' && inView) {
      e.preventDefault();
      takeOver();
      openPalette();
      input.focus();
      input.select();
    }
  });

  new IntersectionObserver(([entry]) => (inView = entry.isIntersecting), { threshold: 0.4 }).observe(win);

  // Left alone: the app's core loop, over and over, only while someone can see it.
  async function waitVisible() {
    while (auto && (!inView || document.hidden)) await sleep(400);
  }

  async function play() {
    let round = 0;
    await sleep(1600);
    while (auto) {
      const q = queries[round++ % queries.length];
      await waitVisible();
      if (!auto) return;
      setQuery('');
      openPalette();
      await sleep(500);
      for (let i = 1; i <= q.length && auto; i++) {
        setQuery(q.slice(0, i));
        await sleep(70 + Math.random() * 70);
      }
      if (!auto) return;
      await sleep(900);
      if (hits.length > 1) {
        current = 1;
        renderResults();
        await sleep(700);
        if (!auto) return;
      }
      openHit(current);
      await sleep(3600);
    }
  }

  if (auto) play();
}
