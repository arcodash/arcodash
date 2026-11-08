#!/usr/bin/env bash
# Native dev stack in one command: infra, migrations, then backend, worker and frontend in the
# background. For the all-Docker stack use `docker compose up --build` instead (same ports).
# Usage: scripts/dev.sh [up | down | status | logs [backend|worker|frontend]]
set -euo pipefail
S="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

case "${1:-up}" in
  up)
    "$S/infra.sh" up
    "$S/migrate.sh" up
    "$S/backend.sh" start
    "$S/worker.sh" start
    "$S/frontend.sh" start
    echo "==> Open http://localhost:3000 (logs: scripts/dev.sh logs backend|worker|frontend)"
    ;;
  down) "$S/local.sh" stop --procs ;;
  status) "$S/local.sh" status ;;
  logs) exec tail -f "/tmp/arcodash-${2:-backend}.log" ;;
  *) echo "Usage: $0 [up | down | status | logs [backend|worker|frontend]]" >&2; exit 1 ;;
esac
