# scripts/

Run arcodash natively (backend, worker and frontend as host processes with hot reload) against
infrastructure in Docker, and run the tests. The scripts only ever talk to databases on
localhost, and none of them deletes data unless the command says so (`infra.sh reset`,
`migrate.sh fresh`).

| Script | What it does |
|---|---|
| `dev.sh [up\|down\|status\|logs <service>]` | Everything below in one command, in the background. |
| `infra.sh [up\|stop\|down\|restart\|reset]` | Postgres, Valkey, the object store and Mailpit from `docker-compose.yml`. `reset` wipes the dev data. |
| `migrate.sh [up\|down\|status\|fresh]` | Runs the migrator against the dev database. |
| `backend.sh [--no-watch\|start\|stop\|restart\|status]` | API on `:8080`; foreground with cargo-watch reload by default. |
| `worker.sh [start\|stop\|restart\|status]` | Background worker; health on `:8090/health`. |
| `frontend.sh [--port N] [start\|stop\|restart\|status]` | Vite dev server on `:3000`. |
| `local.sh [status\|stop] [--procs\|--dbs]` | Lists or stops this checkout's processes and `arcodash*` containers. |
| `test.sh [--unit-only\|--integration-only\|--e2e-only\|--no-unit]` | Test suites; see [testing](../docs/guide/testing.md). |
| `test-connectors.sh [connector] [--no-teardown]` | Connector tests against real databases and emulators. |

`start` runs a service in the background, with pid and log in `/tmp/arcodash-<service>.{pid,log}`.
A script refuses to start when its port is taken; set `APP_PORT` (backend, worker) or `--port`
(frontend) to use another one, and `BACKEND_URL` so the frontend finds a moved backend.

## Ports

| Port | Service |
|---|---|
| 3000, 8080, 8090 | frontend, backend, worker health |
| 5432, 6379, 4443 | dev Postgres, Valkey, object store |
| 8025, 1025 | Mailpit UI and SMTP |
| 5435 | sample database (`docker compose --profile demo up -d demo-db`) |
| 5433, 6380, 5434, 4444 | test stack: Postgres, Valkey, target Postgres, object store |

## Environment

Precedence is the caller's environment, then `.env`, then the defaults. The Docker hostnames in
`.env.example` (`postgres`, `valkey`, `mailpit`, `gcs`) are mapped to `localhost`.

| Variable | Default | Purpose |
|---|---|---|
| `DATABASE_URL` | `postgres://arcodash:arcodash@localhost:5432/arcodash` | Dev database; created by `infra.sh` if missing. |
| `TEST_DATABASE_URL` | `postgres://arcodash:arcodash@localhost:5433/arcodash_test` | Test database. |
| `ENV_FILE` | `<repo>/.env` | Dotenv file to read. |
| `ARCODASH_DATA_DIR` | `<repo>/.data` | Where the dev containers keep their data. |
| `ARCODASH_EXT` | unset | Frontend extension module, relative to `apps/frontend`. |
| `BACKEND_WATCH_PATHS` | unset | Extra paths cargo-watch follows. |
| `WORKER_DATABASE_URL` | `DATABASE_URL` | Database role for the worker, if it differs from the backend's. |

A workspace that builds its own binaries on top of arcodash can reuse these scripts by setting
`CARGO_MANIFEST_PATH`, `BACKEND_PACKAGE`, `MIGRATE_PACKAGE`, `WORKER_PACKAGE` and `WORKER_BIN`
(defaults: this repo's `Cargo.toml` and the `arcodash-*` packages).
