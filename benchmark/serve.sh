#!/usr/bin/env bash
# Serve the benchmark viewer. serve.py regenerates bench-data.json on each
# request, so the page (which polls every 10s) auto-detects and switches to a
# freshly-rendered run and notifies — no restart needed. Usage: serve.sh [port]
set -euo pipefail
cd "$(dirname "$0")"
python3 gen-viewer.py                 # seed the file so the first load is instant
PORT="${1:-8124}" exec python3 serve.py
