// API correctness parity (k6). The SAME library is scanned by BOTH servers, then every read
// endpoint is fetched from each and diffed field-by-field. Item GUIDs are deterministic (MD5 of
// TypeFullName+path, ported from Jellyfin's GetNewItemId), so the same file gets the same Id on
// both — the diff keys by Id, and that Id-parity assumption is itself checked and headlined.
// NOT a shared-DB test: Hermit can't yet open a Jellyfin DB (adoption path unbuilt), so
// "same data" = same library, independently scanned.
//
// Orchestrated by parity.sh (brings up both servers). Direct:
//   HERMIT_URL=http://localhost:18096 JELLYFIN_URL=http://localhost:18097 k6 run parity.js
// Output: results/PARITY.md.
import http from 'k6/http';
import encoding from 'k6/encoding';
import { ENDPOINTS, tokenHeaders, bringUp } from './bench-lib.js';
import { diff } from './parity-diff.js';

const H = __ENV.HERMIT_URL || 'http://localhost:18096';
const J = __ENV.JELLYFIN_URL || 'http://localhost:18097';
const EXPECTED = parseInt(__ENV.EXPECTED_ITEMS || '0', 10);

export const options = {
  setupTimeout: '45m',   // both scans happen in setup()
  scenarios: { parity: { executor: 'shared-iterations', vus: 1, iterations: 1, maxDuration: '2m' } },
};

// Keys that legitimately differ between two independent instances/scans — ignored by the diff so
// the report shows real divergence, not instance noise. Grouped and tunable:
//  - identity/derived: Id/Key/ItemId diverge (independent scans); ImageTags are content hashes
//    that differ by image lib; ServerId/ServerName/Etag are per-instance.
//  - timestamps: Date* are scan-time.
//  - server-local config (System/Info): product name, paths, addresses, host capabilities.
const VOLATILE = new RegExp('^(' + [
  'Id', 'Key', 'ItemId', 'ImageTags', 'ServerId', 'ServerName', 'Etag', 'PlaySessionId',
  // ImageBlurHashes: Hermit generates valid blurhashes, but the string can't byte-match Jellyfin's
  // (its pixels come from Skia's 128x128 downsample+decode) — a documented, accepted divergence.
  'ImageBlurHashes',
  'DateCreated', 'DateModified', 'DateLastSaved', 'DateLastMediaAdded', 'DateLastRefreshed',
  // Per-session/user instance noise: activity timestamps, the client endpoint, and the session's
  // UserId (users get different GUIDs on each independent server).
  'LastActivityDate', 'LastLoginDate', 'LastPlaybackCheckIn', 'RemoteEndPoint', 'UserId',
  // Scheduled-task execution timestamps: when a task last ran differs run-to-run.
  'StartTimeUtc', 'EndTimeUtc',
  'ProductName', 'PackageName', 'WebPath', 'LocalAddress', 'OperatingSystem', 'OperatingSystemDisplayName',
  'SystemArchitecture', 'EncoderLocation', 'StartupWizardCompleted', 'CanSelfRestart',
  'TranscodingTempPath', 'LogPath', 'InternalMetadataPath', 'ItemsByNamePath', 'CachePath', 'ProgramDataPath',
].join('|') + ')$');

// Item-list DTOs omit Path by default, but Path is our stable cross-server align key (same /media
// mount on both). Request it for parity only (Fields is additive — it doesn't shrink the base DTO).
function parityUrl(path) { return path.includes('userId=') ? `${path}&fields=Path` : path; }

// k6 runs setup() and handleSummary() in different JS contexts (module globals don't cross), and
// only handleSummary can write files — but it can't see setup's data. So the comparison runs in
// setup() and emits the finished report as one base64 line; parity.sh decodes it to PARITY.md.
const MARK_A = '===PARITY_MD_BASE64===', MARK_B = '===END===';

function fetchJson(base, ctx, path) {
  const r = http.get(`${base}${parityUrl(path)}`, tokenHeaders(ctx.token));
  let body = null;
  if (r.status === 200) { try { body = r.json(); } catch (e) { body = '<non-JSON>'; } }
  return { status: r.status, body };
}

const CAP = 40;   // max diff lines shown per endpoint, so a wholly-divergent DTO can't flood the report
function fmtDetail(name, out) {
  const lines = [];
  for (const m of out.mismatch) lines.push(`~ ${m.path}  (J=${m.j}  H=${m.h})`);
  for (const p of out.missing) lines.push(`- ${p.path}   (Jellyfin-only, J=${p.j})`);
  for (const p of out.extra) lines.push(`+ ${p.path}   (Hermit-only, H=${p.h})`);
  const shown = lines.slice(0, CAP);
  if (lines.length > CAP) shown.push(`… ${lines.length - CAP} more`);
  return `### \`${name}\`\n\`\`\`diff\n${shown.join('\n')}\n\`\`\``;
}

export function setup() {
  const hc = bringUp(H, 'hermit', EXPECTED);
  const jc = bringUp(J, 'jellyfin', EXPECTED);

  // Id-parity headline: take Jellyfin's first movie id and see if it resolves on Hermit. If item
  // GUIDs were derived identically (MD5 of TypeFullName+path), it would — it doesn't, so this
  // documents the divergence. Item-scoped endpoints below then use EACH server's OWN first movie
  // (ids diverge, so a shared id can't be used) — comparing DTO shape/fields, not necessarily the
  // same title.
  const firstMovie = (base, ctx) => {
    const b = http.get(`${base}/Items?userId=${ctx.userId}&Recursive=true&IncludeItemTypes=Movie&SortBy=SortName&Limit=1`, tokenHeaders(ctx.token)).json();
    return b.Items && b.Items[0] ? b.Items[0].Id : '';
  };
  jc.itemId = firstMovie(J, jc);
  hc.itemId = firstMovie(H, hc);
  let idParity = 'n/a (no movie found)';
  if (jc.itemId) {
    const hs = http.get(`${H}/Items/${jc.itemId}?userId=${hc.userId}`, tokenHeaders(hc.token)).status;
    idParity = hs === 200
      ? `✅ Jellyfin id \`${jc.itemId}\` also resolves on Hermit`
      : `❌ Jellyfin id \`${jc.itemId}\` → HTTP ${hs} on Hermit — item Id derivation diverges (independent scans don't share ids)`;
  }

  const rows = [], details = [];
  for (const e of ENDPOINTS) {
    if (e.name === 'image_primary') continue;   // binary; resize output differs by image lib, not a JSON diff
    const jr = fetchJson(J, jc, e.path(jc)), hr = fetchJson(H, hc, e.path(hc));
    if (jr.status !== 200 || hr.status !== 200) {
      rows.push(`| \`${e.name}\` | ⚠️ status H=${hr.status} J=${jr.status} | — |`); continue;
    }
    const out = { missing: [], extra: [], mismatch: [] };
    diff(jr.body, hr.body, '', out, VOLATILE);
    const n = out.missing.length + out.extra.length + out.mismatch.length;
    if (n === 0) { rows.push(`| \`${e.name}\` | ✅ match | 0 |`); continue; }
    rows.push(`| \`${e.name}\` | ❌ ${n} diff | mismatch:${out.mismatch.length} missing:${out.missing.length} extra:${out.extra.length} |`);
    details.push(fmtDetail(e.name, out));
  }

  const report = [
    '# Hermit vs Jellyfin — API parity',
    '',
    `- **Jellyfin:** \`${__ENV.JELLYFIN_IMAGE || '?'}\`  · Same library, independently scanned.`,
    `- Items scanned: Hermit ${hc.itemsFound} / Jellyfin ${jc.itemsFound}.`,
    `- Deterministic-Id check: ${idParity}`,
    `- Volatile fields ignored: \`${VOLATILE.source}\``,
    '- `~` value mismatch · `-` Jellyfin-only (missing on Hermit) · `+` Hermit-only.',
    '',
    '| Endpoint | Result | Buckets |',
    '|---|---|---|',
    ...rows,
    '- Item-scoped rows (`item_detail`/`ancestors`/`images`/`similar`) use each server\'s own first movie (ids diverge), so they compare DTO shape, not the same title.',
    ...(details.length ? ['', '## Diffs', '', ...details.map((d) => `${d}\n`)] : []),
    '',
  ].join('\n');
  console.log(MARK_A + encoding.b64encode(report) + MARK_B);   // parity.sh decodes this to PARITY.md
  return {};
}

export default function () { /* comparison runs in setup(); iteration is a no-op */ }
