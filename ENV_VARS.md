# Environment variables

Single reference for every environment variable arcodash reads. Variables are grouped by purpose; each table lists the default, whether the variable is required, which process reads it, and what it does.

## How configuration is loaded

- **Process environment.** The backend (`arcodash-backend`) and worker (`arcodash-worker`) read plain environment variables. Both also call `dotenvy` at startup, which loads a `.env` file found in the working directory or a parent. A variable already set in the process environment always wins over the same variable in `.env`.
- **Docker Compose.** `docker-compose.yml` and `docker-compose.prod.yml` pass `.env` to `migrate`, `backend` and `worker` with `env_file: .env`, so every variable below reaches all three. Compose also reads `.env` to interpolate `${...}` values in the compose file itself (ports, build args, Postgres credentials).
- **Local scripts (`scripts/*.sh`).** The caller's environment wins over `.env`, Docker hostnames are mapped to `localhost`, and the scripts refuse to run against a non-loopback database. Their own variables are in [scripts/README.md](scripts/README.md).
- **Which file to copy.** `cp .env.example .env` for local development. `cp .env.selfhost.example .env` for `docker-compose.prod.yml` (see [docs/self-hosting.md](docs/self-hosting.md)). `.env` is git-ignored and excluded from Docker build contexts.
- **Frontend (`VITE_*`).** Vite bakes `VITE_*` values into the JavaScript bundle at build time. Changing them requires rebuilding the frontend (or the `frontend` image). They are not secret: anything in the bundle is visible to every visitor. Vite reads `apps/frontend/.env*`, not the repo-root `.env`.
- **Quoting.** `.env` is parsed by `dotenvy`, Docker Compose and (for the scripts) bash. Quote values that contain spaces, `#`, `$` or parentheses, for example `LDAP_USER_FILTER="(uid={username})"` and `ADMIN_PASSWORD='p@ss$word'`.
- **Value parsing.**
  - Booleans accept `true`, `false`, `1`, `0` (case-insensitive). An invalid boolean is a startup error for `DISABLE_PUBLIC_URLS` and `LOG_SQL`; for the `CONVEYOR_*_ENABLED` flags it is treated as `false` and an error is logged.
  - Values validated at startup fail the process when malformed: `APP_HOST`, `APP_PORT`, `ENCRYPTION_KEY`, `SMTP_PORT`, `SMTP_TLS`, `WORKER_QUEUES`, `FAST_CONCURRENCY`, `LONG_CONCURRENCY`, `LONG_DRAIN_SECS`, `TRUSTED_PROXY_HOPS`.
  - Most other numeric tunables fall back to their default when malformed. An error naming the variable is logged once.
- **Backend and worker must agree.** The five `CONVEYOR_*_ENABLED` routing flags (all except `CONVEYOR_SCHEDULER_ENABLED`) and `MAX_QUERY_ROWS` must have the same value on both processes. Each process publishes a fingerprint of these to Valkey and logs a warning if the other differs. This is a warning only, so a rolling deploy still boots.

## Minimum to run

| Variable | Why | Generate |
|---|---|---|
| `DATABASE_URL` | Postgres connection string | `postgres://user:pass@host:5432/db` |
| `VALKEY_URL` | Valkey (Redis protocol) for the job queue and rate state | `redis://host:6379` |
| `ENCRYPTION_KEY` | Encrypts stored credentials; exactly 64 hex characters | `openssl rand -hex 32` |
| `COOKIE_SECRET` | Signs session cookies | `openssl rand -hex 32` |
| `UPLOAD_BUCKET` | Object storage for CSV/SQLite uploads; backend and worker refuse to start without it | any bucket name (see [Storage](#storage-and-uploads)) |
| `ADMIN_EMAIL`, `ADMIN_PASSWORD` | Creates the first admin on first start (there is no sign-up page) | choose your own |
| `BASE_URL` | Your public frontend origin; leave the default only for local use | `https://arcodash.example.com` |

For `docker-compose.prod.yml` also set `POSTGRES_PASSWORD` (it must match the password inside `DATABASE_URL`) and, to send mail, `SMTP_HOST`.

## Core and database

| Name | Default | Required | Used by | Description |
|---|---|---|---|---|
| `DATABASE_URL` | none | yes | backend, worker, migrate | Postgres connection string for application state. Also read by `rotate_encryption_key`. |
| `VALKEY_URL` | none | yes | backend, worker | Valkey connection string (job queue, rate limiting, job status). The scheme stays `redis://`. `REDIS_URL` is used as a fallback when `VALKEY_URL` is unset. |
| `ENCRYPTION_KEY` | none | yes | backend, worker | 64 hex characters (32 bytes). AES-256-GCM key for data source credentials, destination secrets, per-organization AI keys and embed secrets. See [Security notes](#security-notes). |
| `COOKIE_SECRET` | none | yes | backend | HS256 signing secret for session cookies. An empty value is rejected at startup; the length is not checked, so use at least 32 random bytes. |
| `BASE_URL` | `http://localhost:3000` | no | backend, worker | Public frontend origin. Used as the single allowed CORS origin, in links inside emails and digests, in embed URLs, and for the default SAML endpoints. Use no trailing slash. |
| `APP_HOST` | `0.0.0.0` | no | backend, worker | Bind address (an IP address, not a hostname). The worker binds its health server here too. |
| `APP_PORT` | `8080` | no | backend, worker | Bind port. The worker serves `GET /health` and `GET /metrics` on it, so give the worker a different port when both run on one host. |
| `RUST_LOG` | `info` | no | backend, worker | Log filter in `tracing` `EnvFilter` syntax. An unparseable value falls back to `info`. |
| `DISABLE_PUBLIC_URLS` | `false` | no | backend | Instance-wide kill switch for anonymous public dashboard and embed links. Reported to the frontend in the session response. |
| `ADMIN_EMAIL` | unset | first start | backend | With `ADMIN_PASSWORD`: if no user with this email exists in the default organization, create it in the `admin` group on startup. A no-op once the user exists. Both must be non-empty. |
| `ADMIN_PASSWORD` | unset | first start | backend | See `ADMIN_EMAIL`. Never commit a real password. |
| `METRICS_TOKEN` | unset | no | backend | Bearer token that enables `GET /metrics` on the backend. Unset or empty: the route returns 404. Read on every request. |
| `ARCODASH_SCHEMA_TRACK` | unset | no | migrate | The migrator refuses to run against a database that was migrated by a different migration history. Set to `oss` to claim an unmarked database whose migration ledger has entries this migrator has no file for, only after confirming `DATABASE_URL` points at the right database. |
| `ARCODASH_ALLOW_DESTRUCTIVE_DOWN` | unset | no | migrate | Set to `1` to allow `migrate down` for migrations that cannot be reversed without data loss (the baseline drops every table; the API-key hashing migration invalidates every API key). Never set it in a deployed environment. |

### Self-host compose (`docker-compose.prod.yml`)

These are read by Docker Compose itself (not by the Rust processes) when you run `docker compose -f docker-compose.prod.yml`.

| Name | Default | Required | Description |
|---|---|---|---|
| `POSTGRES_PASSWORD` | none | yes | Password for the bundled Postgres. Compose refuses to start without it. Must match the password in `DATABASE_URL`. |
| `POSTGRES_USER` | `arcodash` | no | Bundled Postgres user. Must match `DATABASE_URL`. |
| `POSTGRES_DB` | `arcodash` | no | Bundled Postgres database. Must match `DATABASE_URL`. |
| `API_BASE_URL` | `http://localhost:8080` | no | URL the browser uses to reach the backend. Passed to the frontend build as `VITE_API_BASE_URL`; changing it requires rebuilding the `frontend` image. |
| `BACKEND_PORT` | `8080` | no | Host port published for the backend; `127.0.0.1:8080` publishes on loopback only. |
| `FRONTEND_PORT` | `80` | no | Host port published for the frontend; `127.0.0.1:80` publishes on loopback only. |

## Queue and worker

| Name | Default | Required | Used by | Description |
|---|---|---|---|---|
| `WORKER_QUEUES` | `default,queries,schemas,scheduled_queries` | no | worker | Comma-separated queues this worker consumes. An unknown name, or a value naming no queues, fails startup. Set it to `scheduled_queries` to dedicate a worker to long-running scheduled queries. |
| `FAST_CONCURRENCY` | `64` | no | worker | Concurrent jobs shared by the `default`, `queries` and `schemas` queues. Must be at least 1. To disable the tier, remove its queues from `WORKER_QUEUES`. |
| `LONG_CONCURRENCY` | `8` | no | worker | Concurrent jobs for `scheduled_queries`, isolated from the fast tier. Must be at least 1. |
| `LONG_DRAIN_SECS` | `180` | no | worker | On shutdown, how long to wait for in-flight `scheduled_queries` jobs before leaving them to lease-expiry recovery. Must be at least 1. The fast tier uses a fixed 30 s grace. |
| `DB_MAX_CONNECTIONS` | `10` | no | backend, worker | Postgres pool size. Every running job holds a connection, so with the default concurrency (64 + 8) the worker logs a warning at startup and jobs queue for a connection (30 s acquire timeout). Raise it to at least `FAST_CONCURRENCY + LONG_CONCURRENCY` for full concurrency. |
| `LOG_SQL` | `false` | no | backend, worker | Log every SQL statement at `info`, with its timing and row counts. Very verbose; turn on only while debugging. |
| `DB_MIN_CONNECTIONS` | `0` | no | backend, worker | Connections kept open while idle. |
| `JOB_HANDLER_TIMEOUT_SECS` | `max(2 x QUERY_TIMEOUT_SECS, 3600)` | no | worker | Wall-clock ceiling on one queue job, so a wedged handler cannot hold its concurrency slot forever. |
| `WORKER_ID` | unset | no | worker | Explicit worker identity written to run and task rows and the heartbeat key. Falls back to `HOSTNAME`, then `HOST`, then a random per-process suffix. Never give two replicas the same value. Container runtimes set `HOSTNAME` per container, so leave this unset in compose. |
| `CONVEYOR_SEND_NOTIFICATION_ENABLED` | `false` | no | backend, worker | Route `send_notification` through the `arcodash-conveyor` task engine instead of the default queue. |
| `CONVEYOR_SEND_DIGEST_ENABLED` | `false` | no | backend, worker | Same, for `send_digest`. |
| `CONVEYOR_SEND_MAIL_ENABLED` | `false` | no | backend, worker | Same, for `send_mail`. |
| `CONVEYOR_EVALUATE_MONITOR_ENABLED` | `false` | no | backend, worker | Same, for the direct or forced `evaluate_monitor` path. Schedule-driven evaluations always stay on the default queue. |
| `CONVEYOR_EXECUTE_QUERY_ENABLED` | `false` | no | backend, worker | Same, for `execute_query` (interactive and scheduled), using the `query-fast` and `query-long` tiers. Those tiers reuse `FAST_CONCURRENCY`, `LONG_CONCURRENCY` and `LONG_DRAIN_SECS`. |
| `CONVEYOR_SCHEDULER_ENABLED` | `false` | no | worker | Run the escalation tick and the digest fan-out as `arcodash-conveyor` scheduled tasks instead of the in-process ticks, which are then skipped. |

All `CONVEYOR_*_ENABLED` flags are off by default and leave the queue-based path unchanged when off.

## Scheduling

| Name | Default | Required | Used by | Description |
|---|---|---|---|---|
| `SCHEDULE_EVERY_SECS_FLOOR` | `60` | no | backend | Minimum interval, in seconds, that `POST` and `PATCH /api/schedules` accept for interval schedules. This is read by the backend only; the worker and the `arcodash-conveyor` crate keep their own fixed 60 s floor. Lowering it lets the API accept intervals the worker will then run, and the backend logs a warning at startup when it differs from 60. |
| `MONITOR_TICK_SECS` | `300` | no | worker | Interval of the monitor evaluation and escalation tick. The in-process tick is floored at 1 s; when `CONVEYOR_SCHEDULER_ENABLED` is on, the scheduled escalation tick is floored at 60 s (a warning is logged when clamped). |
| `RUN_TIMEOUT_SECS` | `1800` | no | worker | Whole-run wall-clock cap for a scheduled run, on top of the per-query `QUERY_TIMEOUT_SECS`. |
| `RUN_LEASE_SECS` | `90` | no | worker | Heartbeat lease of a running run. The sweeper marks a run failed ("worker lost") after the lease expires. |
| `AUTO_PAUSE_THRESHOLD` | `10` | no | worker | Consecutive failures before a schedule is auto-paused and dropped from the scan until resumed. |
| `CONVEYOR_AUTO_PAUSE_THRESHOLD` | `10` | no | backend, worker | Same, for schedules owned by `arcodash-conveyor`. |
| `MONITOR_WAIT_TIMEOUT` | `7200` | no | worker | Hard ceiling, in seconds, a monitor run may stay parked on an in-flight query run before the sweeper fails it. |
| `MONITOR_DEFAULT_STALENESS_SECS` | `900` | no | worker | "Fresh enough" window for a monitor that sets no `max_data_staleness_secs` and has no interval schedule to infer one from. |

A scheduled query runs at least once, not exactly once. If a worker crashes after a query ran against the target database but before its run row is finalized, the sweeper fails that run and the schedule fires again, so the query executes twice. Schedule read-only queries.

## Query execution

| Name | Default | Required | Used by | Description |
|---|---|---|---|---|
| `QUERY_TIMEOUT_SECS` | `300` | no | backend, worker | Hard wall-clock cap on a single query execution. Raise together with `RUN_TIMEOUT_SECS` and `DB_STATEMENT_TIMEOUT_SECS` (for example to `7200`) for long-running workloads. |
| `DB_STATEMENT_TIMEOUT_SECS` | `300` | no | backend, worker | Database-side backstop behind `QUERY_TIMEOUT_SECS`: Postgres `statement_timeout`, MySQL `max_execution_time`, MongoDB `maxTimeMS`. |
| `MAX_QUERY_ROWS` | `10000` | no | backend, worker | Row cap appended as a `LIMIT` to unbounded `SELECT` and `WITH` queries for connectors that support it. Keep identical on backend and worker. |
| `ARCODASH_ALLOW_PRIVATE_CONNECTORS` | unset (guard on) | no | backend, worker | SSRF guard. By default connectors and webhook destinations refuse loopback, private, link-local, metadata, CGNAT and ULA addresses. Set to `1` or `true` only for local use against a database on a private address (the local `.env.example` sets it). An unparseable value leaves the guard on. Never set it on a deployment reachable from untrusted networks. |
| `LIST_QUERIES_MAX_CANDIDATES` | `2000` | no | backend | Cap on rows `GET /api/queries` loads before applying per-user visibility. |
| `LIST_DASHBOARDS_MAX_CANDIDATES` | `2000` | no | backend | Same, for the dashboards list. |
| `LIST_MONITORS_MAX_CANDIDATES` | `2000` | no | backend | Same, for the monitors list. |
| `LIST_SCHEDULES_MAX_CANDIDATES` | `2000` | no | backend | Same, for the schedules list. |

## Authentication

Password login is always enabled, plus each optional method (SAML, LDAP, trusted proxy header) whose configuration is present.

### Internal password login, rate limit

| Name | Default | Required | Used by | Description |
|---|---|---|---|---|
| `LOGIN_RATE_LIMIT_PER_MIN` | `10` | no | backend | Per-IP cap on `POST /api/login`. See [Rate limits](#rate-limits). |

### LDAP / Active Directory

Enabled when both `LDAP_URL` and `LDAP_SEARCH_BASE_DN` are set; with only one set, `POST /api/ldap/login` returns 503.

| Name | Default | Required | Used by | Description |
|---|---|---|---|---|
| `LDAP_URL` | unset | no | backend | `ldap://` or `ldaps://host:port`. |
| `LDAP_SEARCH_BASE_DN` | unset | no | backend | Base DN searched for the user entry. |
| `LDAP_BIND_DN` | unset (anonymous bind) | no | backend | Service account DN used for the search bind. |
| `LDAP_BIND_PASSWORD` | unset | no | backend | Password for `LDAP_BIND_DN`. |
| `LDAP_USER_FILTER` | `(uid={username})` | no | backend | Search filter; `{username}` is escaped per RFC 4515. Use `(sAMAccountName={username})` for Active Directory. |
| `LDAP_EMAIL_ATTRIBUTE` | `mail` | no | backend | Attribute mapped to email. |
| `LDAP_FIRST_NAME_ATTRIBUTE` | `givenName` | no | backend | Attribute mapped to first name. |
| `LDAP_LAST_NAME_ATTRIBUTE` | `sn` | no | backend | Attribute mapped to last name. |
| `LDAP_GROUP_ATTRIBUTE` | unset (no group sync) | no | backend | Attribute (for example `memberOf`) whose values map to arcodash group names by CN. |

### SAML 2.0

| Name | Default | Required | Used by | Description |
|---|---|---|---|---|
| `SAML_IDP_METADATA_XML` | unset | no | backend | The identity provider's metadata XML, pasted as-is. Its presence enables `GET /api/saml/login` and `POST /api/saml/acs`. It carries the SSO URL, entity ID and signing certificate, so no separate URL or certificate variables exist. |
| `SAML_SP_ENTITY_ID` | `{BASE_URL}/api/saml/metadata` | no | backend | This service provider's entity ID. |

### Remote-user header

| Name | Default | Required | Used by | Description |
|---|---|---|---|---|
| `REMOTE_USER_HEADER` | unset (disabled) | no | backend | Name of an HTTP header carrying an already-authenticated email, for use behind a trusted SSO proxy. A request with the header is treated as that user, and the user is created if missing. Only set it behind a proxy that authenticates requests and strips the header from untrusted traffic; anyone who can set it can impersonate any user. |

## Email (SMTP)

When `SMTP_HOST` is unset, no mail is sent: emails are skipped and logged.

| Name | Default | Required | Used by | Description |
|---|---|---|---|---|
| `SMTP_HOST` | unset | no | backend, worker | SMTP relay host for invitations, password resets, alerts and digests. |
| `SMTP_PORT` | `587` | no | backend, worker | SMTP port. |
| `SMTP_TLS` | `wrapper` | no | backend, worker | `wrapper` (implicit TLS, normally port 465), `starttls` (normally port 587) or `none` (plaintext, for a local mail catcher only; credentials are sent unencrypted). The default does not match the default port: for a relay on 587 set `SMTP_TLS=starttls`. |
| `SMTP_USERNAME` | unset | no | backend, worker | SMTP auth user. Credentials are used only when both username and password are set. |
| `SMTP_PASSWORD` | unset | no | backend, worker | SMTP auth password. |
| `SMTP_FROM` | `arcodash@localhost` | no | backend, worker | `From:` address of outgoing mail. |

## Storage and uploads

Uploaded CSV and SQLite files live in an object store that speaks the Google Cloud Storage JSON API. Both compose files run a bundled one (`fake-gcs-server`, files kept on a volume) and point `STORAGE_EMULATOR_HOST` at it; the bucket is created on startup. Without `STORAGE_EMULATOR_HOST` the processes call Google Cloud Storage with credentials from the GCP metadata server, and the bucket must already exist.

| Name | Default | Required | Used by | Description |
|---|---|---|---|---|
| `UPLOAD_BUCKET` | none | yes | backend, worker | Bucket name. Backend and worker fail to start without it. |
| `STORAGE_EMULATOR_HOST` | unset | no | backend, worker | Base URL of the object store, for example `http://gcs:4443`. Set by the compose files and the native scripts. Requests carry no credentials. |
| `MAX_UPLOAD_BYTES` | `26214400` (25 MiB) | no | backend | Upload size cap. Keep it below any request-body limit of a proxy in front of the backend. |
| `UPLOAD_CACHE_DIR` | `<system temp dir>/arcodash-uploads` | no | backend, worker | Local directory caching downloaded uploads for SQLite and CSV queries. |
| `UPLOAD_CACHE_MAX_AGE_SECS` | `604800` (7 days) | no | backend, worker | Age after which a cached upload is pruned on the next cache miss. There is no size cap; age is the only eviction. |
| `UPLOAD_SWEEP_SECS` | `3600` | no | worker | Interval of the orphaned-upload sweep. Floored at 60. |
| `RESULT_RETENTION_DAYS` | `7` | no | worker | Days a stored query result may go unread and unrefreshed before the retention sweep deletes it. Every read or refresh resets the clock. Floored at 1. |
| `RESULT_SWEEP_SECS` | `3600` | no | worker | Interval of the expired-result sweep. Floored at 60. |
| `STORAGE_BACKFILL_SECS` | `60` | no | worker | Interval of the job that copies customer data still held inline in the database into object storage. Inline data is copied, never removed. Floored at 10. |
| `STORAGE_BACKFILL_BATCH` | `200` | no | worker | Rows copied per area on each backfill tick. Floored at 1. |
| `DATA_SOURCE_UPLOAD_RATE_LIMIT_PER_MIN` | `10` | no | backend | Per-user cap on `POST /data_sources/uploads`. See [Rate limits](#rate-limits). |

## AI (bring your own key)

"Ask with AI" and the other AI features are unavailable until a key is configured, either here (instance-wide fallback) or by an admin in AI Settings (per organization, which takes precedence). Prompts contain the data source schema, a few sample values per column, your SQL and error messages; query results are never sent. Admins can turn sample values off in AI Settings. How the AI features behave is described in [docs/guide/ai.md](docs/guide/ai.md).

| Name | Default | Required | Used by | Description |
|---|---|---|---|---|
| `GEMINI_API_KEY` | unset | no | backend, worker | Instance-wide fallback Gemini key. |
| `GEMINI_MODEL` | `gemini-3.8-flash` | no | backend, worker | Gemini model for text-to-SQL. On a 5xx or connect error, up to two other flash models are tried while the time budget allows. |
| `OPENAI_API_KEY` | unset | no | backend, worker | Instance-wide fallback OpenAI key. |
| `OPENAI_MODEL` | `gpt-5.6-terra` | no | backend, worker | OpenAI model for text-to-SQL. |
| `ANTHROPIC_API_KEY` | unset | no | backend, worker | Instance-wide fallback Anthropic key. |
| `ANTHROPIC_MODEL` | `claude-sonnet-5-5` | no | backend, worker | Anthropic model for text-to-SQL. |
| `AI_RATE_LIMIT_PER_MIN` | `10` | no | backend | Default per-user cap on AI SQL generation; an organization can override it in AI Settings. |
| `AI_PROFILE_MAX_BYTES_BILLED` | `104857600` (100 MiB) | no | worker | Byte cap on each BigQuery sample query run while profiling a data source. A table whose sample would exceed it fails for that table instead of scanning more. |

## Rate limits

All limits are per minute, in-process, and keyed per client IP (anonymous routes) or per user. A value that is `0` or unparseable falls back to the default. Each quota is divided by the number of backend replicas so the effective limit across the deployment matches the configured number.

| Name | Default | Keyed by | Applies to |
|---|---|---|---|
| `LOGIN_RATE_LIMIT_PER_MIN` | `10` | IP | `POST /api/login` |
| `ACCOUNT_RECOVERY_RATE_LIMIT_PER_MIN` | `5` | IP | `/forgot_password`, `/reset_password`, `/users/invitation/accept` |
| `PUBLIC_DASHBOARD_RATE_LIMIT_PER_MIN` | `30` | IP | `GET /dashboards/public/{token}` |
| `PUBLIC_EMBED_RATE_LIMIT_PER_MIN` | `30` | IP | `GET /embed/{token}` |
| `DIGEST_UNSUBSCRIBE_RATE_LIMIT_PER_MIN` | `30` | IP | `GET /digest/unsubscribe` |
| `QUERY_EXECUTE_RATE_LIMIT_PER_MIN` | `60` | user | `/query_results`, `/queries/{id}/results`, `/queries/{id}/refresh` |
| `RESULT_DOWNLOAD_RATE_LIMIT_PER_MIN` | `20` | user | CSV and XLSX result downloads |
| `DATA_SOURCE_TEST_RATE_LIMIT_PER_MIN` | `10` | user | `POST /data_sources/test` |
| `DATA_SOURCE_UPLOAD_RATE_LIMIT_PER_MIN` | `10` | user | `POST /data_sources/uploads` |
| `DESTINATION_TEST_RATE_LIMIT_PER_MIN` | `10` | user | `POST /destinations/test` |
| `MONITOR_RUN_RATE_LIMIT_PER_MIN` | `60` | user | `POST /monitors/{id}/run` |
| `MONITOR_NOTIFY_RATE_LIMIT_PER_MIN` | `10` | user | `POST /monitors/{id}/subscriptions/{sub_id}/notify` |
| `USER_INVITE_RATE_LIMIT_PER_MIN` | `10` | user | `POST /users`, `POST /users/{id}/resend_invitation` |
| `CHANGE_PASSWORD_RATE_LIMIT_PER_MIN` | `5` | user | `POST /users/me/password` |

`AI_RATE_LIMIT_PER_MIN` (see [AI](#ai-bring-your-own-key)) is a separate per-user limit.

| Name | Default | Required | Used by | Description |
|---|---|---|---|---|
| `TRUSTED_PROXY_HOPS` | `0` | no | backend | Number of trusted reverse-proxy hops in front of the backend. At `0` the limiter keys on the TCP peer address; otherwise it reads the client IP from `X-Forwarded-For`, counting that many entries from the right. Behind a proxy or load balancer set it to the number of hops, or every request shares one bucket. Leave it at `0` when no trusted proxy is present, because trusting the header otherwise lets a client spoof its address. A non-integer value fails startup. |
| `RATE_LIMIT_REPLICAS` | unset (1) | no | backend | Number of backend replicas to divide each quota by. Set it on any deployment running more than one backend instance, otherwise every quota multiplies by the instance count. Minimum 1. |

## Frontend build (`VITE_*`)

Baked into the bundle at build time. See [How configuration is loaded](#how-configuration-is-loaded).

| Name | Default | Required | Used by | Description |
|---|---|---|---|---|
| `VITE_API_BASE_URL` | empty (same origin) | no | frontend build | Backend API base URL used by browser code. The Docker build takes it from the `VITE_API_BASE_URL` build arg (dev compose hardcodes `http://localhost:8080`; prod compose passes `API_BASE_URL`). `scripts/frontend.sh` requires it to be a loopback address. |

## Development and test variables

The local scripts' variables are in [scripts/README.md](scripts/README.md); test variables are in [docs/guide/testing.md](docs/guide/testing.md).

### Operator utilities

| Name | Default | Description |
|---|---|---|
| `OLD_ENCRYPTION_KEY`, `NEW_ENCRYPTION_KEY` | none (both required) | Inputs to the `rotate_encryption_key` binary. See [Security notes](#security-notes). |

## Advanced tuning

Defaults are suitable for most deployments. Change them only with a reason.

### Job sweeps (worker)

| Name | Default | Description |
|---|---|---|
| `SWEEP_BATCH_LIMIT` | `500` | Maximum rows any one `runs` sweep processes per cycle. Lower than the conveyor limit because these sweeps do per-row database round trips. |
| `ABANDONED_PENDING_SECS` | `3600` | Age after which a run that was enqueued but never claimed is finalized as timed out. Raise it alongside a long `RUN_TIMEOUT_SECS` or a persistently backlogged long tier. |
| `NOTIFICATION_RETRY_BACKOFF_SECS_BASE` | `30` | Backoff before the first retry of a failed notification delivery; the second retry waits 4x as long. A test and tuning knob, not a normal deploy value. |

### `arcodash-conveyor` task engine (backend and worker)

| Name | Default | Description |
|---|---|---|
| `CONVEYOR_LEASE_SECS` | `90` | Lease of a running task before the sweeper reclaims it. |
| `CONVEYOR_SWEEP_INTERVAL_SECS` | `60` | How often the scheduler loop runs its sweep. |
| `CONVEYOR_STALE_PENDING_SECS` | `60` | Age of a pending task before the sweeper treats it as stale and re-enqueues it. |
| `CONVEYOR_ABANDONED_PENDING_SECS` | `3600` | Age of an enqueued but never claimed task before it is finalized as timed out. Must exceed the longest legitimate wait for a worker to claim it. |
| `CONVEYOR_TASK_TIMEOUT_SECS` | `21600` | Hard ceiling on one handler invocation, independent of any timeout the handler enforces itself. Raise it if a handler's own timeout is longer. |
| `CONVEYOR_DEFAULT_RESOURCE_CAP` | `4` | Concurrency cap applied when a task sets a resource key but no explicit cap. |
| `CONVEYOR_MAX_SLEEP_SECS` | `120` | Ceiling on the deadline loop's sleep between ticks, and its fallback when the wake subscription is unavailable. |
| `CONVEYOR_CLAIM_LIMIT` | `100` | Maximum due schedules claimed per deadline tick. |
| `CONVEYOR_SWEEP_BATCH_LIMIT` | `2000` | Maximum rows any one sweep query processes per pass; a backlog past this is picked up on the next pass. |
| `CONVEYOR_TASK_RETENTION_DAYS` | `14` | Days of finished task history kept before pruning. |
| `CONVEYOR_PRUNE_BATCH_LIMIT` | `5000` | Rows deleted per statement in the prune pass. |
| `CONVEYOR_PRUNE_MAX_PER_CYCLE` | `100000` | Ceiling on rows the prune pass deletes in one cycle. |
| `CONVEYOR_LIVE_SNAPSHOT_LIMIT` | `500` | Maximum rows per category in the live diagnostic snapshot. |

## Security notes

- **Never commit `.env`.** It is git-ignored and excluded from Docker contexts. Only the `*.example` templates are tracked. The `COOKIE_SECRET`, `ENCRYPTION_KEY` and admin values in `.env.example` are placeholders for local use; generate new ones for any other environment (`openssl rand -hex 32`).
- **`ENCRYPTION_KEY` rotation.** Stored data source credentials, destination secrets, per-organization AI keys and embed secrets are encrypted with this key. Changing the value without re-encrypting makes those values unreadable, and the affected data sources and destinations stop working. To rotate, stop the backend and worker, take a database backup, then run the offline utility: `OLD_ENCRYPTION_KEY=<old> NEW_ENCRYPTION_KEY=<new> DATABASE_URL=<url> cargo run -p arcodash-backend --bin rotate_encryption_key`. With `docker-compose.prod.yml`: `docker compose -f docker-compose.prod.yml stop backend worker`, then `docker compose -f docker-compose.prod.yml run --rm --no-deps -e OLD_ENCRYPTION_KEY=<old> -e NEW_ENCRYPTION_KEY=<new> --entrypoint /app/rotate_encryption_key backend` (it uses `DATABASE_URL` from `.env`). It re-encrypts everything in one transaction and aborts, changing nothing, if any value decrypts under neither key. Then start the processes with the new `ENCRYPTION_KEY`. Keep a secure copy of the key: it cannot be recovered from the database.
- **`COOKIE_SECRET` rotation** invalidates every active session, so all users must sign in again.
- **`BASE_URL` must be your own domain.** It becomes the allowed CORS origin and the base of links in emails, password resets and embeds. Pointing it at a host you do not control sends those links, including reset tokens, there.
- **`ARCODASH_ALLOW_PRIVATE_CONNECTORS`** disables the SSRF egress guard. Use it only for local development or a trusted private network.
- **`REMOTE_USER_HEADER`** trusts a request header as proof of identity. Enable it only behind a proxy that strips the header from untrusted traffic.
- **`TRUSTED_PROXY_HOPS`** must match the real number of proxies in front of the backend. Too high lets clients spoof their IP; too low collapses clients into the proxy's address.
- **Metrics.** `GET /metrics` on the backend requires `METRICS_TOKEN`. The worker's `/metrics` and `/health` on its own port are not token-gated, so do not publish the worker port.
- **`SMTP_TLS=none`** sends mail and SMTP credentials without encryption. Use it only against a local mail catcher.
