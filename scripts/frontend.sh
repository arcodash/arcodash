#!/usr/bin/env bash
# Frontend dev server (Vite, hot reload) on http://localhost:3000, calling the backend at
# BACKEND_URL (default http://localhost:8080).
# Usage: scripts/frontend.sh [--port N] [start | stop | restart | status]   (no action: foreground)
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=./lib.sh
source "$ROOT/scripts/lib.sh"
PORT=3000
ACTION=fg
USAGE="Usage: $0 [--port N] [start | stop | restart | status]"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --port) [[ $# -ge 2 ]] || { echo "$USAGE" >&2; exit 1; }; PORT="$2"; shift 2 ;;
    fg|start|stop|restart|status) ACTION="$1"; shift ;;
    *) echo "$USAGE" >&2; exit 1 ;;
  esac
done

case "$ACTION" in
  stop|status) _service frontend "$ACTION" ""; exit 0 ;;
esac

export BACKEND_URL="${BACKEND_URL:-http://localhost:8080}"
export VITE_API_BASE_URL="${VITE_API_BASE_URL:-$BACKEND_URL}"
_assert_loopback "$BACKEND_URL"
_assert_loopback "$VITE_API_BASE_URL"
# Optional UI extension module, resolved relative to apps/frontend.
[[ -n "${ARCODASH_EXT:-}" ]] && export ARCODASH_EXT || unset ARCODASH_EXT
echo "==> Frontend on http://localhost:$PORT (backend $BACKEND_URL)"

cd "$ROOT/apps/frontend"
_service frontend "$ACTION" "$PORT" pnpm dev --port "$PORT" --strictPort
