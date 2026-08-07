// Phase-harness bootstrap: bring one server up to a scanned, ready state ONCE
// (wizard → auth → add libraries → wait for the scan to settle), so the
// per-endpoint phase scripts can then hit a warm server without re-scanning.
// Reuses the exact provisioning path the load/parity scripts use (bench-lib).
import { bringUp } from './bench-lib.js';

const TARGET = __ENV.TARGET;                 // 'hermit' | 'jellyfin'
const BASE = __ENV.BASE_URL;
const EXPECTED = parseInt(__ENV.EXPECTED_ITEMS || '0', 10);

export const options = {
  setupTimeout: '45m',                       // setup() waits for the full scan
  scenarios: { boot: { executor: 'shared-iterations', vus: 1, iterations: 1 } },
};

export function setup() {
  const ctx = bringUp(BASE, TARGET, EXPECTED);
  console.log(`[${TARGET}] bootstrap ready: ${ctx.itemsFound} items`);
  return ctx;
}

export default function () { /* provisioning happens in setup(); nothing to loop */ }
