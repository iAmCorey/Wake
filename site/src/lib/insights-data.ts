// A made-up year of activity for the Insights panel. Seeded, so every build draws the
// same picture, and every number on the panel is derived from the same days: the
// overview, the heatmap, the weekly chart and the leaderboards all agree.
import { AGENT_BY_ID } from './agents';

export interface Board {
  name: string;
  icon?: string;
  sessions: number;
  tokens: number | null;
  prompts: number;
}

export interface InsightsData {
  /** 53 × 7 cells, Monday first, oldest week first; null for days still to come. */
  days: ({ date: string; prompts: number } | null)[];
  weeks: { start: string; total: number; byAgent: Record<string, number> }[];
  series: { id: string; name: string; color: string }[];
  hour: number[];
  weekday: number[];
  month: number[];
  totals: { sessions: number; tokens: number; prompts: number; agents: number; projects: number; activeDays: number };
  last7: { sessions: [number, number]; prompts: [number, number]; activeDays: [number, number] };
  streak: { current: number; longest: number };
  busiest: { date: string; prompts: number };
  since: string;
  boards: { agents: Board[]; projects: Board[]; models: Board[] };
}

const iso = (d: Date) => d.toISOString().slice(0, 10);

export function makeInsights(today = new Date()): InsightsData {
  let seed = 20261009;
  const rand = () => (seed = (seed * 16807) % 2147483647) / 2147483647;

  // The grid ends with the current week; Monday is the first row.
  const end = new Date(Date.UTC(today.getFullYear(), today.getMonth(), today.getDate()));
  const dow = (end.getUTCDay() + 6) % 7;
  const start = new Date(end);
  start.setUTCDate(end.getUTCDate() - dow - 52 * 7);

  const days: InsightsData['days'] = [];
  for (let i = 0; i < 53 * 7; i++) {
    const d = new Date(start);
    d.setUTCDate(start.getUTCDate() + i);
    if (d > end) {
      days.push(null);
      continue;
    }
    const week = Math.floor(i / 7);
    const weekday = i % 7;
    // Quiet at first, busier as the year goes on, lighter weekends, a two-week break.
    const ramp = 0.35 + 0.65 * (week / 52);
    const weekend = weekday >= 5 ? 0.3 : 1;
    const holiday = week >= 30 && week <= 31 ? 0.05 : 1;
    // The last stretch is always active, so the streak shows a few days running.
    const recent = (end.getTime() - d.getTime()) / 86400000 < 9;
    const active = recent || rand() < 0.86 * weekend * holiday + 0.04;
    const prompts = active ? Math.max(3, Math.round((8 + rand() * 70) * ramp * (weekday >= 5 ? 0.55 : 1))) : 0;
    days.push({ date: iso(d), prompts });
  }

  // Each week's prompts split between agents; the mix drifts over the year.
  const series = [
    { id: 'claude-code', w: (t: number) => 0.46 - 0.08 * t },
    { id: 'codex', w: (t: number) => 0.12 + 0.24 * t },
    { id: 'cursor', w: (t: number) => 0.22 - 0.14 * t },
    { id: 'gemini', w: () => 0.07 },
    { id: 'opencode', w: (t: number) => 0.03 + 0.04 * t },
  ];
  const otherShare = 0.06;
  const weeks = Array.from({ length: 53 }, (_, wi) => {
    const cells = days.slice(wi * 7, wi * 7 + 7);
    const total = cells.reduce((n, c) => n + (c?.prompts ?? 0), 0);
    const t = wi / 52;
    const weights = series.map((s) => s.w(t) * (0.85 + rand() * 0.3));
    const sum = weights.reduce((a, b) => a + b, 0) / (1 - otherShare);
    const byAgent: Record<string, number> = {};
    let used = 0;
    series.forEach((s, k) => {
      const n = Math.round((total * weights[k]) / sum);
      byAgent[s.id] = n;
      used += n;
    });
    byAgent.other = Math.max(0, total - used);
    return { start: cells.find(Boolean)?.date ?? '', total, byAgent };
  });

  const prompts = days.reduce((n, c) => n + (c?.prompts ?? 0), 0);
  const perAgent = (id: string) => weeks.reduce((n, w) => n + (w.byAgent[id] ?? 0), 0);
  const others = perAgent('other');

  // Agents: the five charted ones plus the long tail that "Other" stands for.
  const agentRows: [string, number, number | null][] = [
    ['claude-code', perAgent('claude-code'), 41000],
    ['codex', perAgent('codex'), 38000],
    ['cursor', perAgent('cursor'), 26000],
    ['gemini', perAgent('gemini'), 30000],
    ['opencode', perAgent('opencode'), 34000],
    ['kimi', Math.round(others * 0.36), null],
    ['copilot', Math.round(others * 0.27), null],
    ['grok', Math.round(others * 0.22), 29000],
    ['dsh', others - Math.round(others * 0.36) - Math.round(others * 0.27) - Math.round(others * 0.22), 33000],
  ];
  const agents: Board[] = agentRows.map(([id, p, perPrompt]) => ({
    name: AGENT_BY_ID[id].name,
    icon: AGENT_BY_ID[id].icon,
    prompts: p,
    sessions: Math.max(1, Math.round(p / (6.5 + rand() * 3))),
    tokens: perPrompt === null ? null : Math.round(p * perPrompt * (0.9 + rand() * 0.2)),
  }));

  const split = (names: string[], shares: number[]): Board[] =>
    names.map((name, i) => {
      const p = Math.round(prompts * shares[i]);
      return {
        name,
        prompts: p,
        sessions: Math.max(1, Math.round(p / (6 + rand() * 4))),
        tokens: Math.round(p * (28000 + rand() * 16000)),
      };
    });
  const projects = split(
    ['acme-web', 'rusty-search', 'pocket-pay', 'blog-engine', 'oss-metrics', 'dotfiles'],
    [0.27, 0.21, 0.16, 0.11, 0.08, 0.04],
  );
  const models = split(
    ['claude-fable-5', 'gpt-5.5', 'composer-2', 'gemini-3-pro', 'kimi-k3', 'glm-5.3'],
    [0.36, 0.25, 0.12, 0.08, 0.05, 0.03],
  );

  // Distributions
  const hourShape = [2, 1, 1, 0, 0, 0, 1, 2, 5, 9, 13, 14, 9, 8, 12, 15, 14, 11, 7, 6, 8, 10, 8, 4];
  const hour = hourShape.map((v) => Math.round((v / 158) * prompts * (0.92 + rand() * 0.16)));
  const weekday = [0, 1, 2, 3, 4, 5, 6].map((wd) => days.reduce((n, c, i) => n + (i % 7 === wd ? (c?.prompts ?? 0) : 0), 0));
  const month = Array.from({ length: 12 }, () => 0);
  for (const c of days) if (c) month[Number(c.date.slice(5, 7)) - 1] += c.prompts;

  // Streaks and the busiest day
  const past = days.filter((c): c is { date: string; prompts: number } => c !== null);
  let longest = 0;
  let run = 0;
  for (const c of past) {
    run = c.prompts > 0 ? run + 1 : 0;
    longest = Math.max(longest, run);
  }
  let current = 0;
  for (let i = past.length - 1; i >= 0 && past[i].prompts > 0; i--) current++;
  const busiest = past.reduce((a, b) => (b.prompts > a.prompts ? b : a));

  // Last 7 days against the 7 before
  const tail = past.slice(-14);
  const sumP = (xs: typeof tail) => xs.reduce((n, c) => n + c.prompts, 0);
  const act = (xs: typeof tail) => xs.filter((c) => c.prompts > 0).length;
  const prev = tail.slice(0, 7);
  const last = tail.slice(7);
  const totalsSessions = agents.reduce((n, a) => n + a.sessions, 0);
  const perSession = prompts / totalsSessions;

  return {
    days,
    weeks,
    series: [...series.map((s) => ({ id: s.id, name: AGENT_BY_ID[s.id].name, color: AGENT_BY_ID[s.id].color }))],
    hour,
    weekday,
    month,
    totals: {
      sessions: totalsSessions,
      tokens: agents.reduce((n, a) => n + (a.tokens ?? 0), 0),
      prompts,
      agents: agents.length,
      projects: 23,
      activeDays: past.filter((c) => c.prompts > 0).length,
    },
    last7: {
      sessions: [Math.round(sumP(last) / perSession), Math.round(sumP(prev) / perSession)],
      prompts: [sumP(last), sumP(prev)],
      activeDays: [act(last), act(prev)],
    },
    streak: { current, longest },
    busiest,
    since: past.find((c) => c.prompts > 0)?.date ?? past[0].date,
    boards: { agents, projects, models },
  };
}
