# Using arcodash

A tour of the main workflow once arcodash is running (see the [README](../../README.md#-quick-start)).
Sign in with the `ADMIN_EMAIL` / `ADMIN_PASSWORD` from `.env`; there is no sign-up page, so
invite other users from the admin pages.

## Connect a data source

**Data Sources → New data source**, pick a connector, fill in the form, **Test connection**, then
**Create**. The form comes from each connector's configuration schema; the
[connector reference](../reference/connectors.md) lists every field. The sample database
(`docker compose --profile demo up -d demo-db`) is a good first target.

Connections to localhost and private networks are refused unless
`ARCODASH_ALLOW_PRIVATE_CONNECTORS=1` is set, which the local `.env.example` does.

## Query

**Queries → New query**, choose the data source, write SQL and **Execute**. Execution is
asynchronous; the UI polls until the result is ready (the [API reference](../reference/api-reference.md)
documents the job contract). **Stop** abandons a running query on arcodash's side, though the
database may keep running it until it finishes or hits its own timeout.

Queries support parameters, snippets, scheduled refresh and version history. A query is a draft
until you publish it.

## Visualize and build dashboards

Add a visualization to any query result (table, line, bar, area, pie, scatter, counter, funnel),
then pin it to a dashboard. Dashboards have a grid layout, dashboard-level parameters,
auto-refresh, sharing links and iframe embeds.

## Monitor the data

Add a monitor on a query to check freshness, null rate, row count or a threshold, and subscribe a
destination (email, Slack, a webhook, PagerDuty and others). An alert fires on a state change and
carries a stats line describing what changed.

A **baseline monitor** has no threshold. Pick a numeric column and a sensitivity (Low, Medium or
High); arcodash learns what is normal from the last 30 checks (median and MAD) and alerts when a
value falls well outside it. It needs 7 checks before it can alert. On a perfectly flat series it
only alerts on moves of roughly 37% (High), 52% (Medium) or 74% (Low) or more.

## AI

Ask questions in plain English, add tiles and monitors from a sentence, explain dashboards and
alerts. See [AI features](ai.md).
