# Connectors

arcodash ships 20 connectors, registered in `ConnectorRegistry::with_db`
(`crates/shared/src/connectors/registry.rs`). `GET /api/data_sources/types` returns them with
their JSON configuration schemas, which the UI turns into the "add data source" form.

| Name | Type id | Driver | Config fields |
|---|---|---|---|
| PostgreSQL | `pg` | `sqlx` (native protocol) | `host`, `port` (5432), `user`, `password`*, `dbname`, `sslmode` (disable/prefer/require, default prefer) |
| Query Results (meta-source) | `query_results` | in-process (queries other data sources' cached results) | none — no external connection |
| MySQL | `mysql` | `sqlx` (native protocol) | `host`, `port` (3306), `user`, `password`*, `db` |
| MariaDB | `mariadb` | `sqlx` (MySQL wire protocol) | same fields as MySQL |
| TiDB | `tidb` | `sqlx` (MySQL wire protocol) | same fields as MySQL |
| SQLite | `sqlite` | `sqlx` | `upload_id` (uploaded file; always opened read-only) |
| Amazon Redshift | `redshift` | `sqlx` (Postgres wire protocol) | same as PostgreSQL; default port 5439 |
| CockroachDB | `cockroachdb` | `sqlx` (Postgres wire protocol) | same as PostgreSQL; default port 26257 |
| ClickHouse | `clickhouse` | `reqwest` (HTTP interface) | `host`, `port` (8123), `user`, `password`*, `db`, `use_https` (default false) |
| Databricks | `databricks` | `reqwest` (SQL Statement Execution API) | `host`, `http_path`, `token`* (PAT), `catalog` (optional), `schema` (optional) |
| Elasticsearch | `es` | `reqwest` (REST API) | `host`, `port` (9200), `user`, `password`*, `use_https` (default false), `index` (optional) |
| MongoDB | `mongodb` | `mongodb` (official Rust driver) | `host`, `port` (27017), `user`, `password`*, `db`, `auth_source` (default admin), `replica_set` (optional) |
| Prometheus | `prometheus` | `reqwest` (HTTP API) | `host`, `port` (9090), `use_https` (default false) |
| InfluxDB (v1) | `influxdb` | `reqwest` (HTTP API) | `host`, `port` (8086), `db`, `user`, `password`*, `use_https` (default false) |
| JSON (generic HTTP) | `json` | `reqwest` | `url`, `user` (optional), `password`* (optional) |
| CSV | `csv` | object storage | `upload_id` (uploaded file), `delimiter` (default `,`) |
| SQL Server (MSSQL) | `mssql` | `tiberius` (pure-Rust TDS, rustls) | `server`, `port` (1433), `user`, `password`*, `db`, `encrypt` (off/optional/required, default required), `trust_server_certificate` (default false), `tds_version` (informational) |
| BigQuery ‡ | `bigquery` | `reqwest` (REST: SA-JWT → OAuth2 → jobs.query) | `project_id`, `service_account_json`*, `location` (optional), `dataset` (optional), `api_endpoint` (optional — local emulator override) |
| Snowflake † | `snowflake` | `reqwest` (SQL API v2, key-pair JWT) | `account`, `user`, `private_key`* (or `password`*), `warehouse`, `database`, `schema`, `role` (optional) |
| Amazon Athena † | `athena` | `aws-sdk-athena` (start → poll → get results) | `aws_access_key`, `aws_secret_key`*, `region`, `s3_output_location`, `database`; `work_group`/Athena `catalog` name (optional — e.g. `AwsDataCatalog`, not a Glue account ID), `api_endpoint` (optional — local emulator override) |

\* Secret fields are encrypted at rest with `ENCRYPTION_KEY` (AES-256-GCM) and never returned by
the API.

Every connector goes through the egress guard, which refuses loopback, private, link-local,
metadata, CGNAT and ULA addresses unless `ARCODASH_ALLOW_PRIVATE_CONNECTORS=1` (see
[ENV_VARS.md](../../ENV_VARS.md#query-execution)).

## How they are tested

Most connectors run against real servers or emulators in `docker-compose.testing.yml` through
`scripts/test-connectors.sh`. BigQuery (‡) runs against an emulator, so its service-account token
exchange is covered by unit tests. Snowflake and Athena (†) have no local emulator: they are
unit-tested and run live only with real account credentials. See [Testing](../guide/testing.md).

## Adding a connector

Implement the `QueryRunner` trait (`crates/shared/src/connectors/mod.rs`) in a new module:
`configuration_schema` (the form), `test_connection`, `get_schema`, `run_query`, and
`network_targets` so the egress guard covers its outbound hosts. Register it in
`ConnectorRegistry::with_db`, add an integration test module to
`crates/shared/tests/connectors.rs` (with a service in `docker-compose.testing.yml` if a container
or emulator exists), and add a row to the table above.
