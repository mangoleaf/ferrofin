#!/usr/bin/env bash
# THE viewer (replaces benchmark/serve :8124 + parity/serve.sh :8123 — one
# page, one port). Serves the REPO ROOT so the page can fetch both the merged
# runs (/suite/results/runs.json) and the full parity ledger
# (/parity/ledger.json).
#   suite/viewer/serve.sh [port]     → http://127.0.0.1:<port>/suite/viewer/
set -euo pipefail
cd "$(dirname "$0")/../.."
PORT="${1:-8125}"
echo "→ http://127.0.0.1:$PORT/suite/viewer/"
exec python3 -m http.server "$PORT" --bind 127.0.0.1
