#!/usr/bin/env bash
# Regenerate the benchmark viewer data and serve it. Usage: benchmark/serve.sh [port]
set -euo pipefail
cd "$(dirname "$0")"
python3 gen-viewer.py
PORT="${1:-8124}"
echo "→ http://127.0.0.1:$PORT/"
exec python3 -m http.server "$PORT" --bind 127.0.0.1
