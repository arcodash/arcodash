#!/usr/bin/env bash
# Connector integration tests (crates/shared/tests/connectors.rs) against the connector services
# in docker-compose.testing.yml plus a local JSON fixture. See docs/guide/testing.md.
# Usage: scripts/test-connectors.sh [connector] [--no-teardown]
#   connector: pg mysql mssql clickhouse mongodb elasticsearch influxdb json bigquery snowflake
#              athena (snowflake/athena need real accounts via TEST_* vars, else skip)
#   --no-teardown: leave the services running afterwards

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
COMPOSE="$REPO_ROOT/docker-compose.testing.yml"
SCRATCH="${ARCODASH_DATA_DIR:-$REPO_ROOT/.data}/connectors"
NO_TEARDOWN=false
CONNECTOR=""
VALID_CONNECTORS="pg mysql mssql clickhouse mongodb elasticsearch influxdb json bigquery snowflake athena"
SERVICES="mysql mariadb clickhouse mongo elasticsearch influxdb mssql target-postgres bigquery"

for arg in "$@"; do
  case $arg in
    --no-teardown) NO_TEARDOWN=true ;;
    --*) ;; # ignore unknown flags
    *) CONNECTOR="$arg" ;;
  esac
done

if [ -n "$CONNECTOR" ]; then
  valid=false
  for c in $VALID_CONNECTORS; do
    [ "$c" = "$CONNECTOR" ] && valid=true && break
  done
  if [ "$valid" = "false" ]; then
    echo "ERROR: unknown connector '$CONNECTOR'. Valid: $VALID_CONNECTORS" >&2
    exit 1
  fi

  case $CONNECTOR in
    pg) SERVICES="target-postgres" ;;
    mysql) SERVICES="mysql mariadb" ;;
    mssql) SERVICES="mssql" ;;
    clickhouse) SERVICES="clickhouse" ;;
    mongodb) SERVICES="mongo" ;;
    elasticsearch) SERVICES="elasticsearch" ;;
    influxdb) SERVICES="influxdb" ;;
    bigquery) SERVICES="bigquery" ;;
    json | snowflake | athena) SERVICES="" ;;
  esac
fi

JSON_SERVER_PID=""

teardown() {
  echo "==> Teardown..."
  if [ -n "$JSON_SERVER_PID" ] && kill -0 "$JSON_SERVER_PID" 2>/dev/null; then
    kill "$JSON_SERVER_PID" 2>/dev/null || true
  fi
  if [ "$NO_TEARDOWN" = "false" ]; then
    if [ -n "$SERVICES" ]; then
      # shellcheck disable=SC2086
      docker compose -f "$COMPOSE" stop $SERVICES 2>/dev/null || true
    fi
  else
    echo "    --no-teardown set: leaving docker stack running"
  fi
}
trap teardown EXIT

# ── 1. scratch dirs ────────────────────────────────────────────────────────────
mkdir -p "$SCRATCH/json_fixture"

# ── 2. JSON fixture HTTP server ────────────────────────────────────────────────
echo '[{"name":"a","n":1},{"name":"b","n":2}]' > "$SCRATCH/json_fixture/data.json"
if [ -z "$CONNECTOR" ] || [ "$CONNECTOR" = "json" ]; then
  python3 -m http.server 5506 --directory "$SCRATCH/json_fixture" &>/dev/null &
  JSON_SERVER_PID=$!
  echo "==> JSON server PID $JSON_SERVER_PID on port 5506"
fi

# ── 3. Docker stack ────────────────────────────────────────────────────────────
if [ -n "$SERVICES" ]; then
  echo "==> Starting connector docker stack..."
  # shellcheck disable=SC2086
  docker compose -f "$COMPOSE" up -d --wait $SERVICES
else
  echo "==> No docker services needed for '$CONNECTOR'"
fi

# ── 4. Extra ES readiness: wait for green/yellow (up to 90s) ──────────────────
if [ -z "$CONNECTOR" ] || [ "$CONNECTOR" = "elasticsearch" ]; then
  echo "==> Waiting for Elasticsearch cluster health..."
  ES_READY=false
  for i in $(seq 1 18); do
    if curl -sf "http://localhost:5504/_cluster/health" 2>/dev/null | grep -qE '"status":"(green|yellow)"'; then
      ES_READY=true
      break
    fi
    echo "    attempt $i/18 — not ready yet, sleeping 5s"
    sleep 5
  done
  if [ "$ES_READY" = "false" ]; then
    echo "ERROR: Elasticsearch did not become healthy within 90s" >&2
    exit 1
  fi
  echo "    Elasticsearch ready"
fi

# ── 5. Seed ────────────────────────────────────────────────────────────────────

# pg (target-postgres, 5434) — doubles as TEST_TARGET_DATABASE_URL's container in scripts/test.sh
seed_pg() {
  echo "    postgres (target-postgres:5434)..."
  psql postgres://target:target@localhost:5434/target_db -c \
    "CREATE TABLE IF NOT EXISTS conn_test (id SERIAL PRIMARY KEY, name TEXT NOT NULL, n INT NOT NULL);" \
    -c "INSERT INTO conn_test (name, n) VALUES ('a', 1), ('b', 2) ON CONFLICT DO NOTHING;" \
    -c "CREATE TABLE IF NOT EXISTS conn_types (id INT PRIMARY KEY, ts_col TIMESTAMP, tstz_col TIMESTAMPTZ, numeric_col NUMERIC(20,4));" \
    -c "INSERT INTO conn_types (id, ts_col, tstz_col, numeric_col) VALUES (1, '2026-08-14 09:00:00', '2026-08-14 09:00:00+00', '12345678901234.5678') ON CONFLICT (id) DO NOTHING;" \
    -c "ALTER TABLE conn_types ADD COLUMN IF NOT EXISTS range_col int4range;" \
    -c "UPDATE conn_types SET range_col = int4range(1, 10) WHERE id = 1;" \
    2>&1 | grep -v "^$" || true
}

# mysql (5500) + mariadb (5501) — mariadb tests live inside crates/shared/tests/connectors.rs's `mod mysql`.
# conn_types covers type decoding: unsigned ints, JSON, TIME, YEAR, BIT, SET, ENUM, BLOB, DECIMAL,
# DATETIME, plus one all-NULL row, since a NULL of any of these must decode like a populated one.
CONN_TYPES_MYSQL_SQL="CREATE TABLE IF NOT EXISTS conn_types (
     row_num INT AUTO_INCREMENT PRIMARY KEY,
     id INT UNSIGNED,
     tiny_u TINYINT UNSIGNED,
     small_u SMALLINT UNSIGNED,
     medium_u MEDIUMINT UNSIGNED,
     big_u BIGINT UNSIGNED,
     j JSON,
     t TIME,
     y YEAR,
     b BIT(8),
     s SET('a','b','c'),
     e ENUM('x','y','z'),
     blob_col BLOB,
     numeric_col DECIMAL(20,4),
     ts_col DATETIME,
     flag_col TINYINT(1)
   );
   INSERT IGNORE INTO conn_types
     (row_num, id, tiny_u, small_u, medium_u, big_u, j, t, y, b, s, e, blob_col, numeric_col, ts_col, flag_col)
   VALUES
     (1, 42, 200, 50000, 10000000, 18446744073709551615, '{\"a\":1}', '12:34:56', 2026, b'10101010', 'a,b', 'x', 0x68656C6C6F, '12345678901234.5678', '2026-08-14 09:00:00', 1),
     (2, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL);
   UPDATE conn_types SET flag_col = 1 WHERE row_num = 1;"

seed_mysql() {
  echo "    mysql (5500)..."
  MYSQL_CID=$(docker compose -f "$COMPOSE" ps -q mysql)
  docker exec "$MYSQL_CID" mysql -utest -ptest testdb -e \
    "CREATE TABLE IF NOT EXISTS conn_test (id INT AUTO_INCREMENT PRIMARY KEY, name VARCHAR(255) NOT NULL, n INT NOT NULL);
     INSERT IGNORE INTO conn_test (id, name, n) VALUES (1,'a',1),(2,'b',2);
     $CONN_TYPES_MYSQL_SQL" 2>/dev/null

  echo "    mariadb (5501)..."
  MARIADB_CID=$(docker compose -f "$COMPOSE" ps -q mariadb)
  docker exec "$MARIADB_CID" mariadb -utest -ptest testdb -e \
    "CREATE TABLE IF NOT EXISTS conn_test (id INT AUTO_INCREMENT PRIMARY KEY, name VARCHAR(255) NOT NULL, n INT NOT NULL);
     INSERT IGNORE INTO conn_test (id, name, n) VALUES (1,'a',1),(2,'b',2);
     $CONN_TYPES_MYSQL_SQL" 2>/dev/null
}

# clickhouse (5502) — d32 (Date32) is added via ALTER ... IF NOT EXISTS so a container reused
# across runs (--no-teardown) gets the column too.
seed_clickhouse() {
  echo "    clickhouse (5502)..."
  curl -sf "http://localhost:5502/?user=test&password=test&database=testdb" \
    --data "CREATE TABLE IF NOT EXISTS conn_test (id UInt32, name String, n Int32) ENGINE = MergeTree() ORDER BY id"
  curl -sf "http://localhost:5502/?user=test&password=test&database=testdb" \
    --data "ALTER TABLE conn_test ADD COLUMN IF NOT EXISTS d32 Date32 DEFAULT toDate32('2026-08-14')"
  curl -sf "http://localhost:5502/?user=test&password=test&database=testdb" \
    --data "INSERT INTO conn_test (id, name, n) VALUES (1,'a',1),(2,'b',2)"
}

# mssql (5507)
seed_mssql() {
  echo "    mssql (5507)..."
  MSSQL_CID=$(docker compose -f "$COMPOSE" ps -q mssql)
  docker exec "$MSSQL_CID" /opt/mssql-tools18/bin/sqlcmd -C -S 127.0.0.1 -U sa -P 'Arcodash1Test!' -Q \
    "IF NOT EXISTS (SELECT * FROM sys.databases WHERE name = 'testdb') CREATE DATABASE testdb;"
  docker exec "$MSSQL_CID" /opt/mssql-tools18/bin/sqlcmd -C -S 127.0.0.1 -U sa -P 'Arcodash1Test!' -d testdb -Q \
    "IF NOT EXISTS (SELECT * FROM sys.tables WHERE name = 'conn_test')
       CREATE TABLE conn_test (id INT PRIMARY KEY, name VARCHAR(255) NOT NULL, n INT NOT NULL);
     IF NOT EXISTS (SELECT * FROM conn_test WHERE id = 1) INSERT INTO conn_test (id, name, n) VALUES (1,'a',1);
     IF NOT EXISTS (SELECT * FROM conn_test WHERE id = 2) INSERT INTO conn_test (id, name, n) VALUES (2,'b',2);
     IF NOT EXISTS (SELECT * FROM sys.tables WHERE name = 'conn_types')
       CREATE TABLE conn_types (id INT PRIMARY KEY, numeric_col DECIMAL(20,4));
     IF NOT EXISTS (SELECT * FROM conn_types WHERE id = 1)
       INSERT INTO conn_types (id, numeric_col) VALUES (1, 12345678901234.5678);"
}

# mongo (5503)
seed_mongodb() {
  echo "    mongo (5503)..."
  MONGO_CID=$(docker compose -f "$COMPOSE" ps -q mongo)
  docker exec "$MONGO_CID" mongosh testdb \
    --username root --password root --authenticationDatabase admin \
    --eval "db.conn_test.drop(); db.conn_test.insertMany([{name:'a',n:1},{name:'b',n:2}]);" \
    --quiet
}

# elasticsearch (5504)
seed_elasticsearch() {
  echo "    elasticsearch (5504)..."
  curl -sf -XPUT "http://localhost:5504/conn_test" \
    -H 'Content-Type: application/json' \
    -d '{"mappings":{"properties":{"name":{"type":"keyword"},"n":{"type":"integer"}}}}' \
    | grep -qE '"acknowledged":true' || echo "    (index may already exist)"
  curl -sf -XPOST "http://localhost:5504/conn_test/_doc?refresh=true" \
    -H 'Content-Type: application/json' -d '{"name":"a","n":1}' >/dev/null
  curl -sf -XPOST "http://localhost:5504/conn_test/_doc?refresh=true" \
    -H 'Content-Type: application/json' -d '{"name":"b","n":2}' >/dev/null
}

# influxdb (5505) — line protocol write
seed_influxdb() {
  echo "    influxdb (5505)..."
  curl -sf -XPOST "http://localhost:5505/write?db=testdb" \
    --data-binary $'conn_test,host=a value=1\nconn_test,host=b value=2'
}

# bigquery emulator (9050) — CREATE TABLE + INSERT via jobs.query (the emulator ignores the
# Authorization header). testdb.conn_test matches every other connector.
seed_bigquery() {
  echo "    bigquery emulator (9050)..."
  # The emulator ignores IF NOT EXISTS and answers 400 "already created" on re-runs; accept that.
  local out
  out=$(curl -s -X POST "http://localhost:9050/bigquery/v2/projects/test/queries" \
    -H 'Content-Type: application/json' \
    -d '{"query":"CREATE TABLE IF NOT EXISTS testdb.conn_test (id INT64, name STRING, n INT64)","useLegacySql":false}')
  case "$out" in
    *'"error"'*"already created"*) ;;
    *'"error"'*) echo "bigquery create failed: $out" >&2; return 1 ;;
  esac
  curl -sf -X POST "http://localhost:9050/bigquery/v2/projects/test/queries" \
    -H 'Content-Type: application/json' \
    -d '{"query":"DELETE FROM testdb.conn_test WHERE true","useLegacySql":false}' \
    >/dev/null
  curl -sf -X POST "http://localhost:9050/bigquery/v2/projects/test/queries" \
    -H 'Content-Type: application/json' \
    -d '{"query":"INSERT INTO testdb.conn_test (id, name, n) VALUES (1, \"a\", 1), (2, \"b\", 2)","useLegacySql":false}' \
    >/dev/null
}

echo "==> Seeding databases..."
for c in pg mysql clickhouse mssql mongodb elasticsearch influxdb bigquery; do
  if [ -z "$CONNECTOR" ] || [ "$CONNECTOR" = "$c" ]; then "seed_$c"; fi
done
echo "==> Seeding done"

echo "==> Running connector tests..."
CARGO_TEST_ARGS=(--manifest-path "$REPO_ROOT/crates/shared/Cargo.toml" --test connectors)
if [ -n "$CONNECTOR" ]; then
  CARGO_TEST_ARGS+=("$CONNECTOR")
fi

TEST_PG_URL="postgres://target:target@localhost:5434/target_db" \
TEST_MYSQL_URL="mysql://test:test@localhost:5500/testdb" \
TEST_MARIADB_URL="mysql://test:test@localhost:5501/testdb" \
TEST_MSSQL_URL="mssql://sa:Arcodash1Test!@localhost:5507/testdb" \
TEST_CLICKHOUSE_URL="http://test:test@localhost:5502/testdb" \
TEST_MONGO_URL="mongodb://root:root@localhost:5503/testdb" \
TEST_ES_URL="http://localhost:5504" \
TEST_INFLUX_URL="http://localhost:5505" \
TEST_INFLUX_DB="testdb" \
TEST_JSON_URL="http://localhost:5506/data.json" \
TEST_BIGQUERY_ENDPOINT="http://127.0.0.1:9050" \
TEST_BIGQUERY_PROJECT_ID="test" \
TEST_BIGQUERY_DATASET="testdb" \
ARCODASH_ALLOW_PRIVATE_CONNECTORS=1 \
  cargo test "${CARGO_TEST_ARGS[@]}" -- --test-threads=1 --nocapture

echo "==> All connector tests passed."
