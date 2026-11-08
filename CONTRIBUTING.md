# Contributing to arcodash

Thanks for considering a contribution.

## Ways to contribute

- Report bugs and request features through [issues](https://github.com/arcodash/arcodash/issues).
- Improve the docs.
- Add or improve a data source connector.
- Pick up an issue labeled `good first issue`.

For questions and ideas, use [Discussions](https://github.com/arcodash/arcodash/discussions).

## Code of Conduct

This project follows the [Contributor Covenant](CODE_OF_CONDUCT.md). By participating you agree to
uphold it.

## Development setup

You need Docker, Rust (`rust-toolchain.toml` pins the version), Node 24 and **pnpm** (not npm or
yarn). Follow [Run from source](README.md#%EF%B8%8F-run-from-source) in the README; the scripts are
documented in [scripts/README.md](scripts/README.md).

## Project layout

- `apps/backend`: Axum REST API.
- `apps/worker`: background jobs (query execution, monitors, notifications).
- `apps/frontend`: React + Vite web app.
- `crates/shared`: shared core library (`arcodash-core`): config, connectors, AI, entities.
- `crates/migration`: database migrations.
- `crates/conveyor`: task queue SDK used by the worker.
- `tests/e2e`: Playwright end-to-end tests.
- `docker/`: Dockerfiles for the compose stacks; `scripts/`: local run and test scripts.
- `docs/`: documentation ([index](docs/README.md)).

## Running tests

```bash
scripts/test.sh --unit-only              # Rust unit tests and Vitest, no Docker
scripts/test.sh                          # unit, integration and e2e against the Docker test stack
scripts/test-connectors.sh [connector]   # connector tests against real databases and emulators
```

See [docs/guide/testing.md](docs/guide/testing.md) for the layers and the test stack.

Add unit tests with every change, and an e2e test when the change is reachable from the UI. If
your change touches a connector or the query path, run the relevant integration script for real
rather than relying on unit tests alone.

## Code style

- Rust: `cargo fmt` and `cargo clippy --workspace --all-targets -- -D warnings` must pass.
- Frontend: `pnpm lint` and `pnpm -r typecheck` must pass.
- Comments explain why, not what. Keep them terse and timeless.

## Commit messages

Use [Conventional Commits](https://www.conventionalcommits.org/): `type(scope): summary`, for
example `feat(connectors): add SSL client certificates for PostgreSQL` or `fix(worker): retry failed notifications`.
Common types: `feat`, `fix`, `docs`, `refactor`, `test`, `chore`.

## Pull request process

1. Fork the repo and branch from `main`.
2. Keep the PR focused; explain what changed and why, and how you tested it.
3. Make sure formatting, lint, and the relevant tests pass locally. CI runs the same checks.
4. Sign the CLA (see below) when the bot asks.
5. A maintainer reviews; address feedback with follow-up commits.

## Adding a connector

Connectors live in `crates/shared/src/connectors/`. Implement the `QueryRunner` trait
(`mod.rs`) in a new module, then register it in `ConnectorRegistry::with_db` in `registry.rs`.
The config schema you declare drives the "add data source" form in the UI. Add unit tests, and an
integration test in `crates/shared/tests/connectors.rs` (with a service in
`docker-compose.testing.yml` if an emulator or container exists). Update
[docs/reference/connectors.md](docs/reference/connectors.md).

## Good first issues

Look for the [`good first issue`](https://github.com/arcodash/arcodash/labels/good%20first%20issue)
label. Comment on the issue before starting so work isn't duplicated.

## Reporting bugs and security issues

Open a [bug report](https://github.com/arcodash/arcodash/issues/new?template=bug_report.yml) with
steps to reproduce, your version, and logs. For security vulnerabilities, do not open a public
issue; follow [SECURITY.md](SECURITY.md).

## License and CLA

arcodash is licensed under the [GNU AGPL-3.0](LICENSE.md). Copyright (C) 2026 Refael Dakar.

Contributions require signing the [Contributor License Agreement](CLA.md). On your first pull
request a bot asks you to sign by commenting; you only do this once. The CLA lets the project
relicense contributions, for example to offer commercial licenses, while the code in this
repository stays available under the AGPL.
