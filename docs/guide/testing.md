# Testing

## Test layers

| Layer | Where | Needs | Run |
|---|---|---|---|
| Rust unit tests | inline `#[cfg(test)]` modules | nothing | `scripts/test.sh --unit-only` |
| Frontend unit tests | Vitest in `apps/frontend` | nothing | `scripts/test.sh --unit-only` |
| Rust integration tests | `apps/*/tests`, `crates/*/tests` | test stack | `scripts/test.sh --integration-only` |
| Connector tests | `crates/shared/tests/connectors.rs` | connector services | `scripts/test-connectors.sh [name]` |
| End-to-end | Playwright in `tests/e2e` | test stack | `scripts/test.sh --e2e-only` |

`scripts/test.sh` with no flag runs the unit, integration and e2e layers; `--no-unit` skips the
first. It starts the app services of `docker-compose.testing.yml`, migrates and seeds the test
database, and stops the services afterwards if it started them. Playwright starts its own backend,
worker and frontend on ports 8080, 8090 and 3000, so stop your dev servers first (`scripts/dev.sh down`),
or set `E2E_REUSE_SERVER=1` to reuse servers that already point at the test stack. Install the
browser once before the first e2e run:

```bash
pnpm --filter @arcodash/e2e exec playwright install --with-deps chromium
```

Tests that need a service check their `TEST_*` variable and print `[SKIP]` when it is unset, so
`cargo test --workspace` is safe without Docker. Database-backed tests share one database: run
them with `--test-threads=1`.

## The test stack

`docker-compose.testing.yml` holds every database and emulator the tests use. All data is in tmpfs
or memory and disappears with the containers.

| Service | Port | Variable |
|---|---|---|
| `postgres` (database `arcodash_test`) | 5433 | `TEST_DATABASE_URL` |
| `valkey` | 6380 | `TEST_REDIS_URL` |
| `mailpit` | 1026 SMTP, 8026 API | `MAILPIT_SMTP_PORT`, `MAILPIT_API_BASE` |
| `gcs` (object store) | 4444 | `STORAGE_EMULATOR_HOST` |
| `target-postgres` (a data source) | 5434 | `TEST_TARGET_DATABASE_URL`, `TEST_PG_URL` |
| `mysql`, `mariadb` | 5500, 5501 | `TEST_MYSQL_URL`, `TEST_MARIADB_URL` |
| `clickhouse` | 5502 | `TEST_CLICKHOUSE_URL` |
| `mongo` | 5503 | `TEST_MONGO_URL` |
| `elasticsearch` | 5504 | `TEST_ES_URL` |
| `influxdb` | 5505 | `TEST_INFLUX_URL`, `TEST_INFLUX_DB` |
| `mssql` | 5507 | `TEST_MSSQL_URL` |
| `bigquery` (emulator) | 9050 | `TEST_BIGQUERY_ENDPOINT`, `TEST_BIGQUERY_PROJECT_ID`, `TEST_BIGQUERY_DATASET` |

The scripts set these variables for you. Running cargo by hand against the stack:

```bash
docker compose -f docker-compose.testing.yml up -d --wait postgres valkey mailpit gcs target-postgres
TEST_DATABASE_URL=postgres://arcodash:arcodash@localhost:5433/arcodash_test \
TEST_REDIS_URL=redis://localhost:6380 \
TEST_TARGET_DATABASE_URL=postgres://target:target@localhost:5434/target_db \
STORAGE_EMULATOR_HOST=http://localhost:4444 \
MAILPIT_SMTP_PORT=1026 MAILPIT_API_BASE=http://localhost:8026 \
  cargo test --workspace --test '*' -- --test-threads=1
```

## Connector tests

`scripts/test-connectors.sh` starts the connector services, seeds each with a `conn_test` table,
serves a JSON fixture on port 5506, and runs `crates/shared/tests/connectors.rs`. Pass a module
name to run one connector: `pg mysql mssql clickhouse mongodb elasticsearch influxdb json bigquery
snowflake athena` (`mysql` also covers MariaDB). `--no-teardown` leaves the services running for
debugging. The script needs `psql`, `python3` and `curl` on your machine.

JSON uses a local fixture. The SQLite and CSV connectors need the app database and object store, so
their tests are in the backend integration tests instead. BigQuery runs against an emulator, which ignores
authentication; the service-account token exchange is covered by unit tests. Snowflake and Athena
have no local emulator and run only against real accounts:

| Connector | Variables |
|---|---|
| Snowflake | `TEST_SNOWFLAKE_ACCOUNT`, `TEST_SNOWFLAKE_USER`, `TEST_SNOWFLAKE_PRIVATE_KEY_PATH`, `TEST_SNOWFLAKE_WAREHOUSE`, `TEST_SNOWFLAKE_DATABASE`, `TEST_SNOWFLAKE_SCHEMA` |
| Athena | `TEST_ATHENA_ACCESS_KEY`, `TEST_ATHENA_SECRET_KEY`, `TEST_ATHENA_REGION`, `TEST_ATHENA_DATABASE`, `TEST_ATHENA_S3_OUTPUT` |
| BigQuery (real account) | `TEST_BIGQUERY_SA_JSON_PATH`, `BIGQUERY_TEST_SA_JSON`, `BIGQUERY_TEST_PROJECT_ID` |

Set `ARCODASH_REQUIRE_CONNECTOR_ENV=1` to make missing variables fail instead of skip.

## Other test variables

| Variable | Purpose |
|---|---|
| `PLAYWRIGHT_BASE_URL`, `BACKEND_URL` | Frontend and backend under test (defaults `http://localhost:3000`, `http://localhost:8080`). |
| `E2E_REUSE_SERVER` | `1` reuses already-running servers instead of starting new ones (ignored in CI). |
| `E2E_EXTRA_ENV_FILE` | Dotenv file merged into the environment of the backend Playwright starts. |
| `ADMIN_EMAIL`, `ADMIN_PASSWORD` | Admin the e2e tests sign in as (default `admin@example.com` / `adminpassword`). |
| `ARCODASH_LIVE_AI`, `ARCODASH_LIVE_AI_PROVIDER` | `1` runs the live AI specs against a real provider key; optionally force `gemini`, `openai` or `anthropic`. |
| `TEST_MYSQL_HOST`, `TEST_MYSQL_PORT` | Live MySQL unit test target. |
| `ARCODASH_TEST_SLOW_DNS` | `1` enables a slow-DNS test of the egress guard. |
| `LIVE_WEBHOOK_URL`, `LIVE_WEBHOOK_TYPE`, `DISCORD_LIVE_DEST_ID`, `DISCORD_LIVE_DB_URL` | Ignored-by-default tests that send to a real destination. |
| `TEST_RATE_LIMIT_REPLICA_DIVISION_PER_MIN` | Used by one rate-limit unit test. |
