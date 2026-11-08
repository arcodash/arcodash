#!/usr/bin/env bash
# Runs the test suites. Integration and e2e start the docker-compose.testing.yml infrastructure
# services and stop them afterwards if this run started them (data volumes are never removed).
# Usage: scripts/test.sh [--unit-only | --integration-only | --e2e-only | --no-unit]
#   unit:        Rust unit tests and frontend Vitest, no Docker
#   integration: Rust integration tests against the test stack
#   e2e:         Playwright; it starts the backend, worker and frontend itself
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=./lib.sh
source "$ROOT/scripts/lib.sh"
COMPOSE_FILE="$ROOT/docker-compose.testing.yml"
COMPOSE=(docker compose -f "$COMPOSE_FILE")
SERVICES=(postgres valkey mailpit gcs target-postgres)
UNIT=true INTEGRATION=true E2E=true

case "${1:-}" in
  "") ;;
  --unit-only) INTEGRATION=false E2E=false ;;
  --integration-only) UNIT=false E2E=false ;;
  --e2e-only) UNIT=false INTEGRATION=false ;;
  --no-unit) UNIT=false ;;
  *) echo "Usage: $0 [--unit-only | --integration-only | --e2e-only | --no-unit]" >&2; exit 1 ;;
esac

STARTED=false
_stop_stack() {
  if $STARTED; then
    echo "==> Stopping the test stack (data kept)"
    "${COMPOSE[@]}" stop "${SERVICES[@]}"
  fi
}
trap _stop_stack EXIT

if $UNIT; then
  echo "==> Rust unit tests"
  # Unset so stack-backed inline tests skip here; they run in the integration step.
  (unset TEST_DATABASE_URL TEST_REDIS_URL TEST_TARGET_DATABASE_URL TEST_MYSQL_HOST; _cargo test --workspace --lib)
  echo "==> Frontend unit tests (Vitest)"
  pnpm --filter @arcodash/frontend test
fi

if $INTEGRATION || $E2E; then
  "${COMPOSE[@]}" ps --status running --services 2>/dev/null | grep -qx postgres || STARTED=true
  echo "==> Starting the test stack"
  "${COMPOSE[@]}" up -d --wait "${SERVICES[@]}"
  _ensure_db "$COMPOSE_FILE" postgres "$TEST_DATABASE_URL"
  echo "==> Migrating the test database"
  DATABASE_URL="$TEST_DATABASE_URL" _cargo run --package "$MIGRATE_PACKAGE" --bin "$MIGRATE_PACKAGE" -- up
  echo "==> Seeding target-postgres"
  "${COMPOSE[@]}" exec -T target-postgres psql -v ON_ERROR_STOP=1 -U target -d target_db -f /dev/stdin \
    < "$ROOT/tests/e2e/seed/target-db.sql"
fi

if $INTEGRATION; then
  echo "==> Rust integration tests"
  TEST_DATABASE_URL="$TEST_DATABASE_URL" \
  TEST_REDIS_URL="redis://localhost:6380" \
  TEST_TARGET_DATABASE_URL="postgres://target:target@localhost:5434/target_db" \
  STORAGE_EMULATOR_HOST="http://localhost:4444" \
  MAILPIT_SMTP_PORT=1026 MAILPIT_API_BASE="http://localhost:8026" \
    _cargo test --workspace --lib --test '*' -- --test-threads=1
fi

if $E2E; then
  echo "==> Playwright e2e tests"
  pnpm test:e2e
fi

echo "==> All tests passed."
