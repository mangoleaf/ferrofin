// Self-check for parity-diff.js. Run: node parity-diff.test.mjs
import assert from 'node:assert/strict';
import { diff, alignKey } from './parity-diff.js';

const VOL = /^(ServerId|DateCreated|Id)$/;
const run = (j, h) => { const out = { missing: [], extra: [], mismatch: [] }; diff(j, h, '', out, VOL); return out; };

// alignKey prefers Path > Name > Id.
assert.equal(alignKey({ Id: 'x', Name: 'A', Path: '/m/a' }), 'Path=/m/a');
assert.equal(alignKey({ Id: 'x', Name: 'A' }), 'Name=A');
assert.equal(alignKey({ Id: 'x' }), 'Id=x');

// Same movie (same Path) but divergent Id + array given in different order → matched by Path, clean.
let o = run(
  { Items: [{ Id: 'J1', Path: '/m/b', Runtime: 10 }, { Id: 'J2', Path: '/m/a', Runtime: 20 }] },
  { Items: [{ Id: 'H9', Path: '/m/a', Runtime: 20 }, { Id: 'H8', Path: '/m/b', Runtime: 10 }] },
);
assert.deepEqual([o.mismatch, o.missing, o.extra], [[], [], []], 'Path-aligned, Id volatile → clean');

// Real field diff on the Path-matched item.
o = run({ Items: [{ Path: '/m/a', Runtime: 20 }] }, { Items: [{ Path: '/m/a', Runtime: 99 }] });
assert.ok(o.mismatch.some((m) => m.path === 'Items[Path=/m/a].Runtime'), 'value diff on aligned item');

// Missing field on Hermit for the matched item, recorded with Jellyfin's value.
o = run({ Items: [{ Path: '/m/a', Genres: [] }] }, { Items: [{ Path: '/m/a' }] });
assert.deepEqual(o.missing.map((x) => x.path), ['Items[Path=/m/a].Genres'], 'field missing on hermit');
assert.equal(o.missing[0].j, '[]', 'missing field carries the Jellyfin value');

// Whole item present only on one side (matched set diff, no index cascade).
o = run({ Items: [{ Path: '/m/a' }, { Path: '/m/b' }] }, { Items: [{ Path: '/m/a' }] });
assert.ok(o.missing.some((p) => p.path === 'Items[Path=/m/b] (whole item)'), 'missing whole item flagged');

// Array of scalars falls back to index/length compare.
o = run({ Tags: ['x', 'y'] }, { Tags: ['x'] });
assert.ok(o.mismatch.some((m) => m.path === 'Tags[]'), 'scalar array length mismatch');

// Genres align by Name (Ids diverge and are volatile).
o = run({ Items: [{ Id: 'J', Name: 'Action' }] }, { Items: [{ Id: 'H', Name: 'Action' }] });
assert.deepEqual([o.mismatch, o.missing, o.extra], [[], [], []], 'genre aligned by Name → clean');

console.log('parity-diff self-check: all assertions passed');
