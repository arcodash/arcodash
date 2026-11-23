# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog 1.1.0](https://keepachangelog.com/en/1.1.0/), and this
project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Snowflake: encrypted private keys (`ENCRYPTED PRIVATE KEY`, AES or 3DES) with a new
  `private_key_passphrase` field.

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
