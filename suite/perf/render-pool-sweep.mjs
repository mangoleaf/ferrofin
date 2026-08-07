// Aggregates results/raw/pool-<n>-summary.json files into one sweep record +
// a readable table. Usage: node render-pool-sweep.mjs <sha> <pool sizes...>
import { readFileSync, writeFileSync } from 'node:fs';

const [sha, ...pools] = process.argv.slice(2);
const runs = pools.map((p) => ({ pool: +p, ...JSON.parse(readFileSync(`results/raw/pool-${p}-summary.json`, 'utf8')) }));

const med = (xs) => {
  const s = xs.filter((x) => x != null).sort((a, b) => a - b);
  return s.length ? s[Math.floor(s.length / 2)] : null;
};

const record = {
  sha,
  when: new Date().toISOString(),
  vus: process.env.BENCH_VUS || '50',
  duration: process.env.BENCH_DURATION || '30s',
  runs: runs.map((r) => {
    const eps = Object.values(r.endpoints);
    return {
      pool: r.pool,
      endpoints: r.endpoints,
      aggregate: {
        med_p50: med(eps.map((e) => e.p50)),
        med_p95: med(eps.map((e) => e.p95)),
        med_p99: med(eps.map((e) => e.p99)),
        total_rps: +eps.reduce((a, e) => a + (e.rps || 0), 0).toFixed(1),
        errored: eps.filter((e) => e.okPct < 100).length,
      },
    };
  }),
};
writeFileSync(`results/pool-sweep-${sha}.json`, JSON.stringify(record, null, 2));

// Aggregate table.
console.log('\npool | med p50 | med p95 | med p99 | total rps | errored eps');
console.log('-----|---------|---------|---------|-----------|------------');
for (const r of record.runs) {
  const a = r.aggregate;
  console.log(`${String(r.pool).padStart(4)} | ${String(a.med_p50).padStart(7)} | ${String(a.med_p95).padStart(7)} | ${String(a.med_p99).padStart(7)} | ${String(a.total_rps).padStart(9)} | ${a.errored}`);
}

// Worst-endpoint drilldown: the 8 slowest endpoints at pool=min, across sizes.
const base = record.runs[0];
const worst = Object.entries(base.endpoints)
  .filter(([, v]) => v.p50 != null)
  .sort(([, a], [, b]) => b.p50 - a.p50)
  .slice(0, 8)
  .map(([k]) => k);
console.log(`\nendpoint p50 by pool size (8 slowest at pool=${base.pool}):`);
console.log(['endpoint', ...record.runs.map((r) => `p=${r.pool}`)].join(' | '));
for (const name of worst) {
  console.log([name, ...record.runs.map((r) => r.endpoints[name]?.p50 ?? '·')].join(' | '));
}
console.log(`\nwrote results/pool-sweep-${sha}.json`);
