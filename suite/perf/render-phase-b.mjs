// Render the Phase B report from results/raw/phaseBmax-*.json (per-endpoint max
// sustainable RPS). Usage: node render-phase-b.mjs <version>
import fs from 'node:fs';

const [version = 'dev'] = process.argv.slice(2);
const dir = 'results/raw';

function load(target) {
  const out = {};
  for (const f of fs.readdirSync(dir)) {
    const m = f.match(new RegExp(`^phaseBmax-${target}-(.+)\\.json$`));
    if (m) out[m[1]] = JSON.parse(fs.readFileSync(`${dir}/${f}`, 'utf8'));
  }
  return out;
}
const H = load('hermit'), J = load('jellyfin');
const names = [...new Set([...Object.keys(H), ...Object.keys(J)])].sort();

const num = (x) => (x == null ? '·' : x);
const rows = names.map((name) => {
  const h = H[name], j = J[name];
  const ratio = (h?.max_rps && j?.max_rps) ? (h.max_rps / j.max_rps) : null;
  return `| \`${name}\` | ${num(h?.max_rps)} | ${num(j?.max_rps)} `
    + `| ${num(h?.p99_at_max)} / ${num(j?.p99_at_max)} `
    + `| ${ratio == null ? '·' : ratio.toFixed(2) + '×'} |`;
}).join('\n');

const md = `# Hermit vs Jellyfin — Phase B (per-endpoint saturation sweep)

- **Hermit:** \`${version}\`  **Jellyfin:** \`${process.env.JELLYFIN_IMAGE || '?'}\`
- Each endpoint driven (open model) at a rising arrival-rate ladder until the
  server drops arrivals or stops returning 200; the last clean rate is its
  **max sustainable throughput** (req/s). Curated endpoint subset.

## Max sustainable throughput (req/s)

| Endpoint | Hermit max RPS | Jellyfin max RPS | p99 at max (H / J, ms) | throughput ratio |
|---|---|---|---|---|
${rows}

> ratio = Hermit max RPS ÷ Jellyfin max RPS (>1 = Hermit sustains more). The
> sweep ladder is coarse (×2 steps), so treat these as order-of-magnitude
> capacity, not exact ceilings.
`;

fs.writeFileSync(`results/phaseB-${version}.md`, md);
