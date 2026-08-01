// Pure JSON-comparison helpers for parity.js — no k6 imports, so they run under node for the
// self-check (parity-diff.test.mjs). diff() walks two trees and buckets differences; arrays are
// matched by a stable key (not index) so two independently-scanned servers whose ids differ still
// line up the SAME item on both sides.

export function kind(x) { return Array.isArray(x) ? 'array' : x === null ? 'null' : typeof x === 'object' ? 'object' : 'leaf'; }
export function brief(x) { const s = JSON.stringify(x); return s && s.length > 80 ? `${s.slice(0, 80)}…` : s; }

// A stable cross-server key for an array element: Path (identical media path) > Name (stable for
// genres/people/libraries) > Id (last resort; diverges between independent scans). Ids differ, so
// Id is only a fallback for elements that have nothing better.
const ALIGN_KEYS = ['Path', 'Name', 'Id'];
export function alignKey(e) {
  if (!e || typeof e !== 'object' || Array.isArray(e)) return null;
  for (const k of ALIGN_KEYS) if (k in e && (typeof e[k] === 'string' || typeof e[k] === 'number')) return `${k}=${e[k]}`;
  return null;
}
// Map key->element if every element has a (unique) align key; else null (fall back to index compare).
function keyed(arr) {
  const m = new Map();
  for (const e of arr) { const k = alignKey(e); if (k === null || m.has(k)) return null; m.set(k, e); }
  return m;
}

// Walk both trees; bucket every difference into out.{mismatch,missing,extra}. `volatile` (RegExp)
// names keys that legitimately differ between instances (ids, dates, paths of the server) — skipped.
export function diff(j, h, path, out, volatile) {
  const tj = kind(j), th = kind(h);
  if (tj !== th) { out.mismatch.push({ path, j: brief(j), h: brief(h) }); return; }
  if (tj === 'object') {
    for (const k of new Set([...Object.keys(j), ...Object.keys(h)])) {
      if (volatile.test(k)) continue;
      const p = path ? `${path}.${k}` : k;
      // Record the present side's value so the report distinguishes null / [] / {} / a real value.
      if (!(k in h)) out.missing.push({ path: p, j: brief(j[k]) });
      else if (!(k in j)) out.extra.push({ path: p, h: brief(h[k]) });
      else diff(j[k], h[k], p, out, volatile);
    }
  } else if (tj === 'array') {
    const jk = keyed(j), hk = keyed(h);
    if (jk && hk) {                          // key-matched: compare the same element on both sides
      for (const key of new Set([...jk.keys(), ...hk.keys()])) {
        const p = `${path}[${key}]`;
        if (!hk.has(key)) out.missing.push({ path: `${p} (whole item)` });
        else if (!jk.has(key)) out.extra.push({ path: `${p} (whole item)` });
        else diff(jk.get(key), hk.get(key), p, out, volatile);
      }
    } else {                                 // no stable key (e.g. array of scalars): compare by index
      if (j.length !== h.length) out.mismatch.push({ path: `${path}[]`, j: `len ${j.length}`, h: `len ${h.length}` });
      for (let i = 0; i < Math.min(j.length, h.length); i++) diff(j[i], h[i], `${path}[${i}]`, out, volatile);
    }
  } else if (j !== h) { out.mismatch.push({ path, j: brief(j), h: brief(h) }); }
}
