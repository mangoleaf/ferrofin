#!/usr/bin/env bash
# Regenerate the ledger and serve the viewer. Usage: parity/serve.sh [port]
set -euo pipefail
cd "$(dirname "$0")"
python3 gen-ledger.py
PORT="${1:-8123}"
echo "→ http://127.0.0.1:$PORT/"
exec python3 -m http.server "$PORT" --bind 127.0.0.1
