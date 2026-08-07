// Render the Phase C report: mixed-load latencies (contention) + memory footprint.
// Usage: node render-phase-c.mjs <version> <vus> <dur>
import fs from 'node:fs';

const [version = 'dev', vus = '?', dur = '?'] = process.argv.slice(2);
const dir = 'results/raw';
const read = (f) => (fs.existsSync(`${dir}/${f}`) ? JSON.parse(fs.readFileSync(`${dir}/${f}`, 'utf8')) : null);

const H = read('phaseC-hermit.json'), J = read('phaseC-jellyfin.json');
const Hm = read('phaseCmem-hermit.json'), Jm = read('phaseCmem-jellyfin.json');
const he = (H && H.endpoints) || {}, je = (J && J.endpoints) || {};
const names = [...new Set([...Object.keys(he), ...Object.keys(je)])].sort();

const n = (x) => (x == null ? '·' : x);
const mib = (b) => (b == null ? '·' : Math.round(b / 1048576) + ' MiB');
const cell = (e) => (e && e.p50 != null ? `${n(e.p50)} / ${n(e.p95)} / ${n(e.p99)}` : '·');
const okc = (h, j) => `${h ? h.okPct : '·'}% / ${j ? j.okPct : '·'}%`;

const rows = names.map((name) => {
  const h = he[name], j = je[name];
  const spd = (h?.p50 && j?.p50) ? (j.p50 / h.p50) : null;
  return `| \`${name}\` | ${cell(h)} | ${cell(j)} | ${okc(h, j)} | ${spd == null ? '·' : spd.toFixed(2) + '×'} |`;
}).join('\n');

const md = `# Hermit vs Jellyfin — Phase C (mixed contention + memory footprint)

- **Hermit:** \`${version}\`  **Jellyfin:** \`${process.env.JELLYFIN_IMAGE || '?'}\`
- **Mixed load:** all endpoints hit concurrently, ${vus} VUs × ${dur} (closed loop).
- This is the CONTENTION view — every endpoint's latency here includes
  interference from the others (shared DB pool, locks). Use Phase A for each
  endpoint's own cost; use this to see how a heavy endpoint drags the rest.

## Footprint (whole run, cgroup-accounted)

| Metric | Hermit | Jellyfin |
|---|---|---|
| memory.peak (high-water, incl. scan) | ${mib(Hm?.mem_peak)} | ${mib(Jm?.mem_peak)} |
| anon working set (end of load) | ${mib(Hm?.mem_anon)} | ${mib(Jm?.mem_anon)} |

## Mixed-load latency (ms, p50 / p95 / p99)

| Endpoint | Hermit | Jellyfin | 200-rate (H / J) | p50 speedup |
|---|---|---|---|---|
${rows}

> Latencies here are inflated by cross-endpoint contention by design; a slow
> endpoint (e.g. one that saturates the DB pool) raises the whole column.
`;

fs.writeFileSync(`results/phaseC-${version}.md`, md);
