// Renders results/raw/phaseD-{hermit,jellyfin}.json into one comparison table.
// Usage: node render-phase-d.mjs <version>
import { readFileSync, writeFileSync, existsSync } from 'node:fs';

const version = process.argv[2] || 'dev';
const load = (t) => {
  const p = `results/raw/phaseD-${t}.json`;
  return existsSync(p) ? JSON.parse(readFileSync(p, 'utf8')) : null;
};
const h = load('hermit');
const j = load('jellyfin');
const fmt = (s) => (s ? `${s.p50} / ${s.p95} / ${s.p99}` : '·');

const steps = ['home', 'library', 'detail', 'images', 'playback'];
let md = `# Phase D — realistic load (${process.env.PHASE_D_VUS || 8} clients, think time)

- **Hermit:** \`${version}\` · window ${process.env.PHASE_D_DUR || '120s'}
- Every VU is its own logged-in device running home → browse → detail → posters → playback
  (incl. the playstate write path), with 1–3 s think time. p50/p95/p99 in ms per step.

| Step | Hermit | Jellyfin |
|---|---|---|
`;
for (const s of steps) {
  md += `| ${s} | ${fmt(h?.steps?.[s])} | ${fmt(j?.steps?.[s])} |\n`;
}
md += `| whole session (ms) | ${h?.sessions ? `${h.sessions.p50} (n=${h.sessions.count})` : '·'} | ${j?.sessions ? `${j.sessions.p50} (n=${j.sessions.count})` : '·'} |\n`;
md += `| non-4xx/5xx rate | ${h?.okPct ?? '·'}% | ${j?.okPct ?? '·'}% |\n`;

writeFileSync(`results/phaseD-${version}.md`, md);
console.log(md);
