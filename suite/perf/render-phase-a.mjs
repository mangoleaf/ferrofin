// Render the Phase A report from the per-endpoint results/raw/phaseA-*.json files.
// Usage: node render-phase-a.mjs <version> <rate> <dur>
import fs from 'node:fs';

const [version = 'dev', rate = '?', dur = '?'] = process.argv.slice(2);
const dir = 'results/raw';

function load(target) {
  const out = {};
  for (const f of fs.readdirSync(dir)) {
    const m = f.match(new RegExp(`^phaseA-${target}-(.+)\\.json$`));
    if (m) out[m[1]] = JSON.parse(fs.readFileSync(`${dir}/${f}`, 'utf8'));
  }
  return out;
}
const H = load('ferrofin'), J = load('jellyfin');
const names = [...new Set([...Object.keys(H), ...Object.keys(J)])].sort();

const n = (x, d = 2) => (x == null ? '·' : Number(x).toFixed(d));
const spd = (h, j) => (h && j ? (j / h) : null);              // >1 ⇒ Ferrofin faster / leaner
const cell = (e) => (e ? `${n(e.p50)} / ${n(e.p95)} / ${n(e.p99)}` : '·');
const drop = (e) => (e && e.dropped > 0 ? ` ⚠️${e.dropped}` : '');

const rows = names.map((name) => {
  const h = H[name], j = J[name];
  const latSpd = spd(h?.p50, j?.p50);
  const cpuSpd = spd(h?.cpu_us_per_req, j?.cpu_us_per_req);   // >1 ⇒ Ferrofin uses less CPU
  return { name, h, j, latSpd, cpuSpd };
});

const table = rows.map(({ name, h, j, latSpd, cpuSpd }) =>
  `| \`${name}\` | ${cell(h)}${drop(h)} | ${cell(j)}${drop(j)} `
  + `| ${n(h?.cpu_us_per_req, 1)} / ${n(j?.cpu_us_per_req, 1)} `
  + `| ${latSpd == null ? '·' : n(latSpd) + 'x'} | ${cpuSpd == null ? '·' : n(cpuSpd) + 'x'} |`,
).join('\n');

const md = `# Ferrofin vs Jellyfin — Phase A (isolated, open-model per endpoint)

- **Ferrofin:** \`${version}\`  **Jellyfin:** \`${process.env.JELLYFIN_IMAGE || '?'}\`
- **Model:** open (constant arrival rate), one endpoint at a time, ${rate} req/s for ${dur} after warm-up.
- **CPU/req:** container cgroup \`cpu.stat usage_usec\` delta over the run, minus idle baseline, ÷ requests.
- Isolated ⇒ each row is that handler's own latency + CPU, with no cross-endpoint contention.

## Latency (ms, p50 / p95 / p99) and CPU cost

| Endpoint | Ferrofin lat | Jellyfin lat | CPU µs/req (H / J) | lat speedup | CPU efficiency |
|---|---|---|---|---|---|
${table}

> "lat speedup" = Jellyfin p50 ÷ Ferrofin p50 (>1 = Ferrofin faster). "CPU efficiency" =
> Jellyfin µs/req ÷ Ferrofin µs/req (>1 = Ferrofin burns less CPU per request). ⚠️N = N
> dropped arrivals (endpoint could not sustain the offered rate ⇒ treat its row as
> saturated, not a clean latency).
`;

fs.writeFileSync(`results/phaseA-${version}.md`, md);
