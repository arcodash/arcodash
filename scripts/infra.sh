#!/usr/bin/env bash
# Local infrastructure from docker-compose.yml: Postgres, Valkey, the object store and Mailpit.
# Usage: scripts/infra.sh [up | stop | down | restart | reset]
#   stop keeps the containers, down removes them (both keep data); reset wipes the dev data.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=./lib.sh
source "$ROOT/scripts/lib.sh"
COMPOSE_FILE="$ROOT/docker-compose.yml"
COMPOSE=(docker compose -f "$COMPOSE_FILE")
SERVICES=(postgres valkey gcs mailpit)
DATA_DIR="${ARCODASH_DATA_DIR:-$ROOT/.data}"

case "${1:-up}" in
  stop) "${COMPOSE[@]}" stop "${SERVICES[@]}"; exit 0 ;;
  down) "${COMPOSE[@]}" down; exit 0 ;;
  restart) "${COMPOSE[@]}" stop "${SERVICES[@]}" ;;
  reset)
    echo "==> Wiping $DATA_DIR/{postgres,valkey,gcs} (local dev data is lost)"
    "${COMPOSE[@]}" rm -sf postgres valkey gcs
    rm -rf "${DATA_DIR:?}/postgres" "${DATA_DIR:?}/valkey" "${DATA_DIR:?}/gcs"
    echo "    done; next: scripts/infra.sh up && scripts/migrate.sh up"
    exit 0
    ;;
  up|start) ;;
  *) echo "Usage: $0 [up | stop | down | restart | reset]" >&2; exit 1 ;;
esac

"${COMPOSE[@]}" up -d --wait postgres valkey
"${COMPOSE[@]}" up -d gcs mailpit
# A DATABASE_URL naming another database gets it created here.
_resolve_database_url
_ensure_db "$COMPOSE_FILE" postgres "$DATABASE_URL"

cat <<INFO
==> Infrastructure is up
  postgres      localhost:5432  (arcodash / arcodash, db arcodash)
  valkey        localhost:6379
  object store  http://localhost:4443
  mailpit       http://localhost:8025  (SMTP localhost:1025)
INFO
