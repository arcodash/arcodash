# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog 1.1.0](https://keepachangelog.com/en/1.1.0/), and this
project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `arcodash-worker clear-inline-data [--confirm] [--batch N]`: re-verifies each inline copy against
  object storage and clears it. A dry run by default; only verified rows are touched.
- A background backfill copies data held inline in Postgres to object storage and verifies it
  (referenced results, monitor observations, dashboard narratives, recent mail bodies, AI profile
  values).
- `RESULT_MAX_DECODED_BYTES` (default 64 MiB): cap on the uncompressed size of a stored query result, enforced on write and on read.
- Stored query results carry a SHA-256 digest (`query_results.data_sha256`) that is verified before the object is decoded; a mismatch is refused. Objects written before the digest existed still load.
- `RESULT_RETENTION_DAYS` (default 7): stored results unused for that long are deleted.
- `STORAGE_BACKFILL_RETRY_SECS` (default 3600): wait before a backfill pass with skipped rows repeats.
- Snowflake: encrypted private keys (`ENCRYPTED PRIVATE KEY`, AES or 3DES) with a new
  `private_key_passphrase` field.

### Changed

- Query results and other data produced by running queries (monitor observations, dashboard
  narratives, mail bodies, AI profile sample values) are stored in the object store
  (`UPLOAD_BUCKET`) instead of Postgres.
- Upgrade: once the backfill has finished, run `docker compose run --rm worker clear-inline-data`
  (a dry run that only counts), then `docker compose run --rm worker clear-inline-data --confirm`,
  to free the inline copies. The dry run's `verified` counts show what `--confirm` would clear; run
  `--confirm` once they match the rows you expect and `not_verified` and `no_copy` are 0. It
  re-verifies each row against the object store, so running it early is safe.
- API responses carry `Cache-Control: no-store` by default.
- Error text stored or returned to clients has customer values scrubbed.

### Fixed

- Data source form renders multi-line fields (Snowflake private key, BigQuery service account
  JSON) as a textarea, so pasted PEM keys keep their line breaks.

## [0.2.0] - 2026-11-21

### Added

- Dashboard tiles show the age of the result they render, in a tile footer.
- Dashboard header line with the oldest result on the page and a summary of the checks behind it.
- Check badge and monitor bell on tiles whose query has checks; the bell opens the monitor with
  the tile and dashboard as context and Baseline preselected.
- Verified queries: admins can mark a query verified (recorded in the audit log); its tiles show
  a shield.
- Tile menu with Explore this tile, which opens the tile's data over Query Results.
- New results pill on dashboards, refreshing only the tiles whose result changed.
- Monitors show the data age the last check evaluated, and a Refresh data button that reruns the
  watched query without evaluating the condition.

### Changed

- A monitor's latest incident collapses to one line once the monitor stops firing.

## [0.1.0] - 2026-11-08

Initial open-source release.

### Added

- 20 data source connectors: PostgreSQL, MySQL, MariaDB, TiDB, SQL Server, SQLite, Redshift,
  CockroachDB, ClickHouse, Databricks, Elasticsearch, MongoDB, Prometheus, InfluxDB, BigQuery,
  Snowflake, Amazon Athena, JSON over HTTP, CSV upload, and the Query Results meta-source.
- SQL editor with parameters, snippets, and query versions.
- Visualizations and dashboards.
- Monitors and data quality checks (freshness, null rate, row count) with alerting.
- Alert destinations: email, Slack, webhook, PagerDuty, Mattermost, Microsoft Teams, Discord,
  Google Chat, and Datadog.
- AI features with your own Anthropic, OpenAI or Gemini key: ask questions in plain English,
  AI-generated queries, widgets and visualizations, alert explanations, and dashboard narratives.
- Sharing and embeds for dashboards.
- Authentication: password, SAML 2.0, LDAP/Active Directory, and trusted proxy header.
- Admin area and audit log.
- REST API.

[Unreleased]: https://github.com/arcodash/arcodash/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/arcodash/arcodash/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/arcodash/arcodash/releases/tag/v0.1.0
