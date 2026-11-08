#!/usr/bin/env bash
# Backend API on http://localhost:${APP_PORT:-8080}. Needs scripts/infra.sh and scripts/migrate.sh.
# Usage: scripts/backend.sh [--no-watch | start | stop | restart | status]
#   no argument: foreground, reloading on change if cargo-watch is installed
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=./lib.sh
source "$ROOT/scripts/lib.sh"
ACTION="${1:-fg}"
RUN=(run --manifest-path "$CARGO_MANIFEST_PATH" --package "$BACKEND_PACKAGE" --bin "$BACKEND_PACKAGE")

case "$ACTION" in
  stop|status) _service backend "$ACTION" "" ; exit 0 ;;
  fg|--no-watch|start|restart) ;;
  *) echo "Usage: $0 [--no-watch | start | stop | restart | status]" >&2; exit 1 ;;
esac

_load_app_env APP_HOST APP_PORT
export APP_HOST="${APP_HOST:-127.0.0.1}" APP_PORT="${APP_PORT:-8080}"
echo "==> Backend on http://localhost:$APP_PORT (database $DATABASE_URL)"

if [[ "$ACTION" == fg ]] && command -v cargo-watch >/dev/null; then
  WATCH=(-w "$ROOT/apps/backend/src" -w "$ROOT/crates")
  for p in ${BACKEND_WATCH_PATHS:-}; do WATCH+=(-w "$p"); done
  _service backend fg "$APP_PORT" cargo watch "${WATCH[@]}" -x "${RUN[*]}"
fi
[[ "$ACTION" == --no-watch ]] && ACTION=fg
_service backend "$ACTION" "$APP_PORT" cargo "${RUN[@]}"
