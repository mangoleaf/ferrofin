#!/usr/bin/env bash
# One viewer for the merged suite (replaces benchmark/serve.sh :8124 + parity/serve.sh :8123).
# Serves suite/ so the page can fetch /results/runs.json and /viewer/index.html.
#   suite/viewer/serve.sh [port]     → http://127.0.0.1:<port>/viewer/
set -euo pipefail
cd "$(dirname "$0")/.."
PORT="${1:-8125}"
echo "→ http://127.0.0.1:$PORT/viewer/"
exec python3 -m http.server "$PORT" --bind 127.0.0.1
