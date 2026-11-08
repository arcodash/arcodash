#!/usr/bin/env bash
# Background worker (queries, monitors, alerts). Needs scripts/infra.sh and scripts/migrate.sh.
# Its health endpoint listens on http://localhost:${APP_PORT:-8090}/health.
# Usage: scripts/worker.sh [start | stop | restart | status]   (no argument: foreground)
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=./lib.sh
source "$ROOT/scripts/lib.sh"
ACTION="${1:-fg}"

case "$ACTION" in
  stop|status) _service worker "$ACTION" ""; exit 0 ;;
  fg|start|restart) ;;
  *) echo "Usage: $0 [start | stop | restart | status]" >&2; exit 1 ;;
esac

# .env's APP_PORT is the backend's, so the worker uses 8090 unless the caller sets one.
CALLER_PORT="${APP_PORT:-}"
_load_app_env WORKER_DATABASE_URL
export APP_PORT="${CALLER_PORT:-8090}"
# WORKER_DATABASE_URL lets the worker connect with a different role than the backend.
if [[ -n "${WORKER_DATABASE_URL:-}" ]]; then
  export DATABASE_URL="$(_localize_dsn "$WORKER_DATABASE_URL")"
  _assert_local_dsn "$DATABASE_URL"
fi
echo "==> Worker health on http://localhost:$APP_PORT/health"

_service worker "$ACTION" "$APP_PORT" \
  cargo run --manifest-path "$CARGO_MANIFEST_PATH" --package "$WORKER_PACKAGE" --bin "$WORKER_BIN"
