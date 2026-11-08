#!/usr/bin/env bash
# Runs the migrator against the local dev database (or DATABASE_URL, which must be loopback).
# Usage: scripts/migrate.sh [up | down | status | fresh]   (fresh drops every table first)
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=./lib.sh
source "$ROOT/scripts/lib.sh"
ACTION="${1:-up}"

_resolve_database_url
_assert_local_dsn "$DATABASE_URL"

echo "==> Migrating ($ACTION): $DATABASE_URL"
_cargo run --package "$MIGRATE_PACKAGE" --bin "$MIGRATE_PACKAGE" -- "$ACTION"
