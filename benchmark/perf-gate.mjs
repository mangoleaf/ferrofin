// Perf-gate comparator: diff the current perfgate-hermit-<name>.json results
// against perf-baseline.json on p50, p95, AND p99. Two modes:
//
//   node perf-gate.mjs compare    <baselineFile> <factor> <name...>
//   node perf-gate.mjs rebaseline <baselineFile> <vus> <secs> <name...>
//
// compare prints a before/after table (all three percentiles) to STDERR and the
// space-separated names of regressed endpoints to STDOUT — so perf-gate.sh can
// re-run just those once to rule out noise. Exit 0 normally; exit 2 only on a
// hard error (missing baseline) so the shell can distinguish "regression" (names
// on stdout, exit 0) from "couldn't run" (exit 2) and never silently pass.
//
// An endpoint counts as regressed if ANY of p50/p95/p99 exceeds factor× baseline,
// or its 200-rate < 100% (bad > 0), or it produced no data. Median-only gating
// hides tail regressions — that's the whole point (plan 04).
import { readFileSync, writeFileSync } from 'node:fs';

export const PCTS = ['p50', 'p95', 'p99'];

// Pure decision: given a baseline row {p50,p95,p99}, a current result
// {p50,p95,p99,ok,bad}, and the factor, return which checks tripped. Empty ⇒ ok.
// base=null ⇒ endpoint not in baseline (caller treats as "skipped", not a fail).
export function classify(base, cur, factor) {
  if (!cur || !cur.ok) return ['nodata'];
  const tripped = [];
  for (const p of PCTS) {
    const ratio = base[p] > 0 ? cur[p] / base[p] : (cur[p] > 0 ? Infinity : 1);
    if (ratio > factor) tripped.push(p);
  }
  if (cur.bad > 0) tripped.push('200%');   // any non-200 ⇒ 200-rate < 100%
  return tripped;
}

const fmt = (n) => (n == null ? '—' : (+n).toFixed(1));
const RAW = (name) => `results/raw/perfgate-hermit-${name}.json`;
const loadCurrent = (name) => { try { return JSON.parse(readFileSync(RAW(name), 'utf8')); } catch { return null; } };

function rebaseline(baselineFile, vus, secs, names) {
  const endpoints = {};
  for (const name of names) {
    const cur = loadCurrent(name);
    if (!cur || !cur.ok) { console.error(`rebaseline: no data for ${name} — aborting`); process.exit(2); }
    if (cur.bad) { console.error(`rebaseline: ${name} had ${cur.bad} non-200s — refusing to baseline a broken endpoint`); process.exit(2); }
    endpoints[name] = { p50: cur.p50, p95: cur.p95, p99: cur.p99 };
  }
  writeFileSync(baselineFile, JSON.stringify({ params: { vus: +vus, secs: +secs }, endpoints }, null, 2) + '\n');
  console.error(`baselined ${names.length} endpoints @ ${vus} VUs × ${secs}s → ${baselineFile}`);
}

function compare(baselineFile, factor, names) {
  let baseline;
  try { baseline = JSON.parse(readFileSync(baselineFile, 'utf8')); }
  catch { console.error(`perf-gate: no baseline at ${baselineFile} — run \`./perf-gate.sh --rebaseline\` first`); process.exit(2); }

  const bp = baseline.params || {};
  console.error(`perf-gate: factor ${factor}×, baseline @ ${bp.vus ?? '?'} VUs × ${bp.secs ?? '?'}s`);
  console.error('endpoint'.padEnd(24) + 'p50 base→cur (×)'.padEnd(22) + 'p95 base→cur (×)'.padEnd(22) + 'p99 base→cur (×)'.padEnd(22) + '200%  verdict');

  const regressed = [];
  for (const name of names) {
    const base = (baseline.endpoints || {})[name];
    const cur = loadCurrent(name);

    if (!cur || !cur.ok) { regressed.push(name); console.error(name.padEnd(24) + 'NO DATA (k6 produced no measured 200s)'); continue; }
    if (!base) { console.error(name.padEnd(24) + `${fmt(cur.p50)}/${fmt(cur.p95)}/${fmt(cur.p99)}   (no baseline — skipped)`); continue; }

    const tripped = classify(base, cur, factor);
    const rate200 = (100 * cur.ok / (cur.ok + cur.bad)).toFixed(0);
    const cols = PCTS.map((p) => {
      const ratio = base[p] > 0 ? cur[p] / base[p] : Infinity;
      return `${fmt(base[p])}→${fmt(cur[p])} (${ratio.toFixed(2)}${ratio > factor ? '!' : ''})`.padEnd(22);
    }).join('');
    console.error(name.padEnd(24) + cols + `${rate200}%`.padEnd(6) + (tripped.length ? `FAIL ${tripped.join(',')}` : 'ok'));
    if (tripped.length) regressed.push(name);
  }
  process.stdout.write(regressed.join(' '));
}

// CLI (skipped when imported by the test, which has no argv[2]).
const [, , mode, baselineFile, ...rest] = process.argv;
if (mode === 'rebaseline') { const [vus, secs, ...names] = rest; rebaseline(baselineFile, vus, secs, names); }
else if (mode === 'compare') { compare(baselineFile, parseFloat(rest.shift()), rest); }
else if (mode) { console.error(`unknown mode: ${mode}`); process.exit(2); }
