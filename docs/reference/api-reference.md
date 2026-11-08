# API Reference

arcodash exposes a REST API under `/api/*`. This page follows the router
(`apps/backend/src/api/router.rs`).

## Conventions

- **Cookie auth** — the normal browser session: `POST /api/login` sets an HTTP-only session
  cookie (signed JWT), used automatically by subsequent requests. It also sets a readable
  `csrf_token` cookie; every non-GET request on a cookie session must echo it in an
  `X-CSRF-Token` header.
- **API key auth** — for scripts/integrations: send `Authorization: Key <key>` as a request
  header, or append `?api_key=<key>` to the URL. Both a per-user key (`users.api_key`, or the
  dedicated `api_keys` table) and per-query API keys are supported. Requests authenticated this
  way are also exempt from CSRF checks (see `apps/backend/src/middleware/csrf.rs`).
- **Anonymous** — a handful of routes need no session at all (login, SAML/LDAP entry
  points, password reset, public dashboards). Marked "none" below.
- **Admin-only** — enforced in the handler, not just hidden in the UI; calling as a non-admin
  returns an authorization error.
- **Jobs / polling contract** — query execution is asynchronous. `POST /query_results` or
  `POST /queries/{id}/results` enqueues a job and returns a job id. Poll `GET /api/jobs/{id}`
  until `status` reaches a terminal value; the response includes `query_result_id` once done (or
  `error` on failure). `DELETE /api/jobs/{id}` requests best-effort cancellation. A job id is an
  opaque, unguessable UUID, and `GET`/`DELETE /jobs/{id}` are limited to the user who submitted the
  job and to admins; other callers get `404`.
- Selected routes carry their own per-IP or per-user rate limit on top of normal auth — see
  [`ENV_VARS.md`](../../ENV_VARS.md#rate-limits) for the `*_RATE_LIMIT_PER_MIN` env vars and which route
  each one guards.

---

## Auth / session

| Method | Path | Auth | Purpose |
|---|---|---|---|
| POST | `/api/login` | none (rate-limited per IP) | Password login; sets the session cookie. |
| POST | `/api/logout` | cookie/API key | Clear the session cookie. |
| GET | `/api/session` | cookie/API key | Current user + org + feature flags (`client_config`, e.g. `disable_public_urls`). |
| GET | `/api/saml/login` | none | Start SAML SP-initiated login (enabled only when `SAML_IDP_METADATA_XML` is set). |
| POST | `/api/saml/acs` | none | SAML assertion consumer service; auto-provisions the user. |
| POST | `/api/ldap/login` | none | LDAP/AD bind+search login (enabled only when `LDAP_URL` + `LDAP_SEARCH_BASE_DN` are set). |
| POST | `/api/forgot_password` | none (rate-limited per IP) | Request a password reset email. |
| POST | `/api/reset_password` | none (rate-limited per IP) | Complete a password reset. |

Remote-user header auth (`REMOTE_USER_HEADER`) has no dedicated route — it's resolved from the
configured header on ordinary authenticated requests when enabled; see
[`ENV_VARS.md`](../../ENV_VARS.md).

## Queries

| Method | Path | Auth | Purpose |
|---|---|---|---|
| GET | `/api/queries` | cookie/API key | List queries visible to the user. |
| POST | `/api/queries` | cookie/API key | Create a query. |
| GET | `/api/queries/favorites` | cookie/API key | List the current user's starred queries. |
| POST | `/api/queries/generate_sql` | cookie/API key | "Ask with AI" text-to-SQL generation (needs an AI provider key, system-wide or per organization). Returns SQL text and never executes it. |
| POST | `/api/queries/ask` | cookie/API key (rate-limited per user) | Agentic "Ask with AI": streams SQL generation, a read-only preview run, and repair attempts as Server-Sent Events (see below). |
| GET | `/api/queries/{id}` | cookie/API key | Get a query. |
| POST | `/api/queries/{id}` | cookie/API key | Update a query. |
| DELETE | `/api/queries/{id}` | cookie/API key | Delete (archive) a query. |
| POST | `/api/queries/{id}/favorite` | cookie/API key | Star a query. |
| DELETE | `/api/queries/{id}/favorite` | cookie/API key | Unstar a query. |
| GET | `/api/queries/{id}/parameter_values` | cookie/API key | Resolve query-based dropdown parameter options. |
| GET | `/api/queries/{id}/version` | cookie/API key | Version history. |
| GET | `/api/queries/{id}/visualizations` | cookie/API key | List visualizations attached to a query. |
| POST | `/api/queries/{id}/revert` | cookie/API key | Revert to a prior version. |
| POST | `/api/queries/{id}/fork` | cookie/API key | Duplicate a query as a new draft. |
| GET | `/api/queries/{id}/results` | cookie/API key (rate-limited per user) | Latest cached result for the query. |
| POST | `/api/queries/{id}/results` | cookie/API key (rate-limited per user) | Execute the saved query; returns a job id (see polling contract above). |
| POST | `/api/queries/{id}/refresh` | cookie/API key (rate-limited per user) | Force re-execution, bypassing the result cache. |
| GET | `/api/queries/{id}/acl` | cookie/API key | List per-object ACL grants (view/modify). |
| POST | `/api/queries/{id}/acl` | cookie/API key | Grant an ACL entry. |
| DELETE | `/api/queries/{id}/acl/{perm_id}` | cookie/API key | Revoke an ACL entry. |

### `POST /api/queries/ask`

Body: `{"data_source_id": 1, "question": "...", "existing_sql": null, "error": null}`. `existing_sql`
and `error` are optional; send them to repair or refine a draft. The response is
`text/event-stream`; each event has a name and a JSON `data` payload:

| Event | Payload |
|---|---|
| `status` | `{text}` progress line. |
| `draft` | `{attempt, sql, explanation}` SQL generated by the model. |
| `executing` | `{attempt}` a read-only draft is being previewed. |
| `preview` | `{attempt, columns, rows, row_count, truncated}` preview rows (never sent back to the model). |
| `repair` | `{attempt, error}` the draft failed; another attempt follows (unless out of attempts). |
| `final` | `{sql, explanation, served_by, attempts, tokens_in, tokens_out, executed, error}` terminal. `explanation` is the model's text for `sql`. `executed` is true when `sql` ran in the preview without error (zero rows counts); otherwise `error` holds the last problem (SQL error, `preview timed out`, blocked or not-read-only message, or a provider failure on a later attempt) and is `null` when executed. |
| `profile_refreshing` | `{data_source_id}` a draft failed because a table or column no longer exists and a background schema refresh started. Sent at most once per stream, before `final`; the stream then ends with its normal `final` carrying the error. The server does not re-ask: to retry, poll `GET /api/data_sources/{id}/schema` until `refreshing` is false, then submit the question again (the dashboard does this once automatically). |
| `failed` | Terminal error. |

Only read-only SQL is ever executed for the preview. AI usage (`GET /api/organization/ai/usage`)
records these calls under the `ask` feature. The usage response carries `totals`, `by_provider`, `by_feature` (`{feature, calls, tokens_in, tokens_out, error_rate}`) and `by_day`. Every one of those carries `cost_usd` (list-price estimate; `null` when no call in the bucket used a priced model). The response also has `by_user` (`{user_id, email, name, calls, tokens_in, tokens_out, cost_usd}`, sorted by cost; `user_id` is `null` for background jobs such as schema descriptions; alert explanations are attributed to the monitor owner) and `budget` (`{monthly_usd, spent_month_usd, resets_at}`; `monthly_usd` is `null` with no cap, and spend is for the current calendar month in UTC regardless of `days`).

## Query results / jobs

| Method | Path | Auth | Purpose |
|---|---|---|---|
| POST | `/api/query_results` | cookie/API key (rate-limited per user) | Execute an ad-hoc (unsaved) query against a data source; returns a job id. |
| GET | `/api/query_results/{id}` | cookie/API key | Fetch a stored result. Append `.csv`/`.tsv`/`.xlsx` to the id segment (e.g. `/api/query_results/123.csv`) to download in that format instead of JSON. |
| GET | `/api/jobs/{id}` | cookie/API key | Poll job status; see the polling contract above. |
| DELETE | `/api/jobs/{id}` | cookie/API key | Request best-effort job cancellation. |

## Data sources

| Method | Path | Auth | Purpose |
|---|---|---|---|
| GET | `/api/data_sources` | cookie/API key | List data sources the user can access. Each item includes `profiling` (an AI profile run is in progress) and `profile_failed` (the last run failed). |
| POST | `/api/data_sources` | admin | Create a data source. |
| GET | `/api/data_sources/types` | cookie/API key | List every registered connector type + its JSON config schema (drives the "add data source" form). |
| GET | `/api/data_sources/{id}` | cookie/API key | Get a data source (never returns decrypted secrets). |
| POST | `/api/data_sources/{id}` | admin | Update a data source. |
| DELETE | `/api/data_sources/{id}` | admin | Delete a data source. |
| GET | `/api/data_sources/{id}/schema` | cookie/API key | The cached table/column schema (for the query editor's schema browser): `{schema, refreshing, refresh_error}`. Never contacts the source during the request. `?refresh=true`, or a first load with nothing cached (for callers allowed to refresh), starts a background refresh and returns immediately; poll the plain `GET` until `refreshing` is false. `refresh_error` is the last schema-refresh failure reason, else `null`. |
| POST | `/api/data_sources/{id}/pause` | admin | Pause scheduled query execution against this source. |
| POST | `/api/data_sources/{id}/resume` | admin | Resume it. |
| GET | `/api/data_sources/{id}/ai_profile` | admin | The stored AI profile (per-table/column stats and sample values) used to build Ask with AI prompts: `{profile, built_at, supported, building, current, phase, last_error, metered}`. `supported`: the source type can be profiled; `building`: true while a profile run is in progress (either phase); `current`: the profile has an entry for every table of the cached schema (AI uses the profile either way, limited to tables and columns that still exist); `phase`: `"schema"`, `"data"` or `null` (idle) while a run is in progress; `last_error`: the last run's failure reason, else `null`; `metered`: the source type bills per query (`bigquery`, `athena`, `snowflake`, `redshift`), so samples are only collected on Rebuild. Each profile table also carries `data_profiled_at` and `error`. |
| POST | `/api/data_sources/{id}/ai_profile/rebuild` | admin | Full rebuild: refresh the schema and re-sample every table. `202 {"status":"queued"}`, or `409` if a run is already in progress. |
| POST | `/api/data_sources/test` | cookie/API key (rate-limited per user) | Test connectivity with a given config, before or after saving. |

Creating, updating and deleting data sources is admin-only; reads are scoped to what the current
user's groups grant access to.

## Dashboards

| Method | Path | Auth | Purpose |
|---|---|---|---|
| GET | `/api/dashboards` | cookie/API key | List dashboards. |
| POST | `/api/dashboards` | cookie/API key | Create a dashboard. |
| GET | `/api/dashboards/tags` | cookie/API key | List distinct tags in use. |
| GET | `/api/dashboards/favorites` | cookie/API key | List the current user's starred dashboards. |
| GET | `/api/dashboards/{slug}` | cookie/API key | Get a dashboard (with its widgets). |
| POST | `/api/dashboards/{slug}` | cookie/API key | Update a dashboard. |
| DELETE | `/api/dashboards/{slug}` | cookie/API key | Delete (archive) a dashboard. |
| POST | `/api/dashboards/{slug}/share` | cookie/API key | Enable a public secret-token share link. |
| DELETE | `/api/dashboards/{slug}/share` | cookie/API key | Revoke the share link. |
| POST | `/api/dashboards/{id}/narrative` | cookie/API key (rate-limited per user) | "Explain" a dashboard: generates an AI summary from the latest cached results of its widgets you can read (no queries are run) and stores it. `summary` is plain text (at most 300 characters); `body` is sanitized Markdown (at most 1500 characters; only `[title](#widget-<id>)` links) or `null` on older rows; `highlights` is empty on new rows. Returns `200` tagged by `status`: `ok` returns `{status, narrative: {id, dashboard_id, created_at, source, summary, body, highlights: [{widget_id, text}], confidence, model}}`; `disabled`, `not_configured`, `invalid_key`, `quota_exceeded`, `rate_limited` (with `retry_after_secs`) and `generation_failed` return `{status, message}`. Other statuses: `403` if the organization isn't entitled to AI features, `429` with code `ai_budget_exceeded` once the monthly AI budget is reached, `404` if you cannot view the dashboard. Needs `dashboard_narratives` and `include_sample_values` both on in AI Settings. |
| GET | `/api/dashboards/{id}/narrative` | cookie/API key | Your most recent narrative for the dashboard, as `{narrative: {...}}` with the same `narrative` object shape as above. `404` before the first run or without dashboard access. |
| POST | `/api/dashboards/{id}/ai_widget` | cookie/API key (rate-limited per user) | "Add with AI": turns a prompt into a draft widget. Streams Server-Sent Events (see below). |
| POST | `/api/dashboards/{id}/favorite` | cookie/API key | Star a dashboard. |
| DELETE | `/api/dashboards/{id}/favorite` | cookie/API key | Unstar a dashboard. |
| GET | `/api/dashboards/{id}/acl` | cookie/API key | List per-object ACL grants. |
| POST | `/api/dashboards/{id}/acl` | cookie/API key | Grant an ACL entry. |
| DELETE | `/api/dashboards/{id}/acl/{perm_id}` | cookie/API key | Revoke an ACL entry. |

### `POST /api/dashboards/{id}/ai_widget`

Body: `{"data_source_id": 1, "prompt": "signups by country, last 30 days"}`. Runs the same Ask loop as
`POST /api/queries/ask` against the data source, picks a visualization for the verified result, then
creates a query, a visualization and a widget in the dashboard's draft in one transaction. The
widget is invisible to published readers until the dashboard is published. The response is
`text/event-stream`; it carries the `status`, `draft`, `executing`, `preview`, `repair`,
`profile_refreshing` and `final` events described above, plus:

| Event | Payload |
|---|---|
| `visualizing` | `{}` the SQL ran; a chart type is being chosen. If the suggestion fails the widget is created as a table. |
| `created` | `{query_id, visualization_id, widget_id, viz_type, row_count}` terminal success; sent after the query's first run finishes (or after a 30 s wait), so the tile has data once it renders. A failed or slow first run does not undo the widget. |
| `failed` | `{status, message}` terminal error; nothing was created. |

Permissions: edit access to the dashboard, execute access to the data source, and the `create_query`
and `edit_query` permissions. Errors before the stream starts: `400` for an empty prompt, `404` if
you cannot view or edit the dashboard (or it does not exist), `403` if you lack the `create_query` or
`edit_query` permission, `429` with code `ai_budget_exceeded` once the monthly AI budget is reached. Only read-only SQL is saved.
Usage is recorded under the `ask` and `auto_visualize` features.

The created query is tagged `ai-widget`. Deleting that widget, or discarding dashboard changes that drop it, also deletes its query and visualization, but only if the query was never edited, has no other visualization, widget, monitor, schedule, favorite, embed or share, and you may delete it; otherwise only the widget goes.

## Public

| Method | Path | Auth | Purpose |
|---|---|---|---|
| GET | `/api/embed/{token}` | none (rate-limited per IP) | Anonymous read of a dashboard or visualization shared via a public link. Returns 404 if the token is unknown, revoked or expired, or public links are off. |
| GET | `/api/dashboards/public/{token}` | none (rate-limited per IP) | Older dashboard-only form of the above. Returns 404 if `DISABLE_PUBLIC_URLS=true`. |

## Embeds

| Method | Path | Auth | Purpose |
|---|---|---|---|
| POST | `/api/embeds` | cookie/API key | Create a public link for a dashboard or visualization (`object_type`, `object_id`, optional `expires_at`, `locked_params`, `frame_ancestors`). |
| GET | `/api/embeds?object_type=&object_id=` | cookie/API key | List the links for one object. |
| GET | `/api/embeds/active_count?query_id=` | cookie/API key | Number of live public links that show a query's results. |
| DELETE | `/api/embeds/{id}` | cookie/API key | Revoke a link. |
| GET, POST | `/api/orgs/me/embed_settings` | cookie/API key (admin to change) | Read or change whether public links are allowed. |

## Widgets

| Method | Path | Auth | Purpose |
|---|---|---|---|
| POST | `/api/widgets` | cookie/API key | Add a widget (visualization or text box) to a dashboard. |
| POST | `/api/widgets/{id}` | cookie/API key | Update a widget (position/size/text). |
| DELETE | `/api/widgets/{id}` | cookie/API key | Remove a widget. |

## Visualizations

| Method | Path | Auth | Purpose |
|---|---|---|---|
| POST | `/api/visualizations` | cookie/API key | Create a visualization for a query. |
| POST | `/api/visualizations/{id}` | cookie/API key | Update a visualization's config. |
| DELETE | `/api/visualizations/{id}` | cookie/API key | Delete a visualization. |

## Monitors

| Method | Path | Auth | Purpose |
|---|---|---|---|
| GET | `/api/monitors` | cookie/API key | List monitors. |
| POST | `/api/monitors` | cookie/API key | Create a monitor (threshold on a query's result). |
| GET | `/api/monitors/{id}` | cookie/API key | Get a monitor. |
| POST | `/api/monitors/{id}` | cookie/API key | Update a monitor. |
| DELETE | `/api/monitors/{id}` | cookie/API key | Delete a monitor. |
| POST | `/api/monitors/{id}/mute` | cookie/API key | Mute notifications without disabling the check. |
| POST | `/api/monitors/{id}/unmute` | cookie/API key | Unmute. |
| GET | `/api/monitors/{id}/subscriptions` | cookie/API key | List destinations subscribed to this monitor. |
| POST | `/api/monitors/{id}/subscriptions` | cookie/API key | Subscribe a destination. |
| DELETE | `/api/monitors/{id}/subscriptions/{sub_id}` | cookie/API key | Unsubscribe (nested path form). |
| DELETE | `/api/monitor_subscriptions/{id}` | cookie/API key | Unsubscribe (flat path form). |
| POST | `/api/queries/{id}/suggest_monitors` | cookie/API key | AI monitor suggestions for a query (see below). |
| POST | `/api/monitors/ask` | cookie/API key | AI-drafted monitor from a prompt, streamed (see below). |
| GET | `/api/monitors/{id}/observations?limit=` | cookie/API key (monitor view permission) | Recent check results, oldest first: `[{observed_at, value, state, explanation, run_id}]`. `limit` defaults to 100 and is clamped to 1..=500. |

### `POST /api/queries/{id}/suggest_monitors`

Body: `{"prompt": "alert me when refund rate passes 5%"}`; `prompt` is optional. Without it, AI proposes
monitors for the query's published SQL and its latest published result columns. With it, AI drafts only
what the prompt asks for. Nothing is created; each suggestion is applied through `POST /api/monitors`.

`200` with `{"status": "ok", "suggestions": [...], "already_covered": [...], "unsupported": "...", "served_by": "..."}`.
A suggestion is `{name, kind, column, sensitivity, op, value, definition, why, confidence}`.
`already_covered` lists existing monitors that watch a column AI would otherwise suggest, and
`unsupported` is a reason when the prompt cannot be built from this query's result. An AI failure
returns `200` with a tagged `{"status": "<failure>", "message": ...}` body instead of `ok`.

`409` with code `query_unpublished` if the query has no published version, or `no_result` if the
published version has never run. Needs view access to the query, execute access to its data source, the `create_monitor`
permission and the AI entitlement. Usage is recorded under the `monitor_suggestions_query` feature.

### `POST /api/monitors/ask`

Body: `{"data_source_id": 1, "prompt": "alert me when daily signups drop"}`. Runs the Ask loop against
the data source to write and run a query for the prompt, then drafts one monitor on its result. The
response is `text/event-stream` with the `status`, `draft`, `executing`, `preview`, `repair`,
`profile_refreshing`, `final` and `failed` events described above, plus:

| Event | Payload |
|---|---|
| `suggesting` | `{}` the SQL ran; a monitor is being drafted. |
| `suggested` | `{sql, data_source_id, suggestion}` terminal success. `suggestion` has the shape above. |

This route creates nothing: no query, no monitor, no subscription. The caller reviews the draft and
saves it with the regular create calls (query, publish, `POST /api/monitors`, subscribe). Requires
execute access to the data source, the `create_query` and `create_monitor` permissions and the AI
entitlement. Errors before the stream starts match `POST /api/queries/ask`, including `429` with code
`ai_budget_exceeded`.

**Monitor options.** `kind` is `threshold` (default) or `baseline`; `sensitivity` is `low`, `medium`
(default) or `high`. A `baseline` monitor requires `column` and forbids `op` and `value`. Updating
`options` merges into the stored options; a `null` value deletes that key.

**Monitor and dashboard-tile responses** include `kind` and, for a baseline monitor with fewer
than 7 observations, `learning: {have, need}`. Tiles also carry `incident_summary`.

## Destinations

| Method | Path | Auth | Purpose |
|---|---|---|---|
| GET | `/api/destinations/types` | cookie/API key | List notification destination types + config schema (Email, Slack, Webhook, PagerDuty, Mattermost, Microsoft Teams, Discord, Google Chat, Datadog). |
| GET | `/api/destinations` | cookie/API key | List configured destinations. |
| POST | `/api/destinations` | admin | Create a destination. |
| GET | `/api/destinations/{id}` | cookie/API key | Get a destination. |
| POST | `/api/destinations/{id}` | admin | Update a destination. |
| DELETE | `/api/destinations/{id}` | admin | Delete a destination. |

## Users

| Method | Path | Auth | Purpose |
|---|---|---|---|
| GET | `/api/users` | cookie/API key (view_users) | List every user in the org — name, email, status and group ids. Any `view_users` holder sees the whole directory, including admins. |
| POST | `/api/users` | admin | Invite a user. |
| GET | `/api/users/invitation/verify` | none | Validate an invitation token (pre-session). |
| POST | `/api/users/invitation/accept` | none (rate-limited per IP) | Accept an invitation and set a password. |
| GET | `/api/users/{id}` | cookie/API key (self) or view_users | Get a user. |
| POST | `/api/users/{id}` | cookie/API key (self) or admin | Update a user (profile, or admin-managed fields). |
| DELETE | `/api/users/{id}` | admin | Delete/disable a user. |
| POST | `/api/users/{id}/regenerate_api_key` | cookie/API key (self) or admin | Rotate a user's API key. |

## Groups

| Method | Path | Auth | Purpose |
|---|---|---|---|
| GET | `/api/groups` | cookie/API key | List groups. |
| POST | `/api/groups` | admin | Create a group. |
| GET | `/api/groups/{id}` | cookie/API key | Get a group. |
| POST | `/api/groups/{id}` | admin | Update a group. |
| DELETE | `/api/groups/{id}` | admin | Delete a group. |
| GET | `/api/groups/{id}/members` | cookie/API key | List members. |
| POST | `/api/groups/{id}/members` | admin | Add a member. |
| DELETE | `/api/groups/{id}/members/{user_id}` | admin | Remove a member. |
| GET | `/api/groups/{id}/data_sources` | cookie/API key | List data sources assigned to the group. |
| POST | `/api/groups/{id}/data_sources` | admin | Assign a data source to the group. |
| DELETE | `/api/groups/{id}/data_sources/{ds_id}` | admin | Unassign it. |

## Snippets

| Method | Path | Auth | Purpose |
|---|---|---|---|
| GET | `/api/query_snippets` | cookie/API key | List org-wide SQL snippets (any member reads all). |
| POST | `/api/query_snippets` | cookie/API key (`create_query` permission) | Create a snippet. |
| GET | `/api/query_snippets/{id}` | cookie/API key | Get a snippet. |
| POST | `/api/query_snippets/{id}` | cookie/API key (`create_query` permission) | Update a snippet. |
| DELETE | `/api/query_snippets/{id}` | cookie/API key (`create_query` permission) | Delete a snippet. |

## Events (audit log)

| Method | Path | Auth | Purpose |
|---|---|---|---|
| GET | `/api/events` | admin | Read the audit log. |

## Organization / admin

| Method | Path | Auth | Purpose |
|---|---|---|---|
| GET | `/api/organization` | admin | Get org settings. |
| POST | `/api/organization` | admin | Update org settings. `settings` is merged into the stored object, not replaced — a key the request omits keeps its current value rather than being deleted. |
| GET | `/api/organization/ai` | admin | Get "Ask with AI" settings (provider keys, rate limit override, `fallback_enabled`, `include_sample_values`, `explain_alerts`, `dashboard_narratives`, `monthly_budget_usd`). AI alert analysis runs only when `explain_alerts` and `include_sample_values` are both on; if `include_sample_values` is off, alerts carry the stats line only and no query results go to the AI provider. Dashboard summaries and digest briefs run only when `dashboard_narratives` (default on) and `include_sample_values` are both on. |
| POST | `/api/organization/ai` | admin | Update AI settings. `monthly_budget_usd` sets a monthly spend cap in USD (`null` clears it; a value that is zero, negative or not finite returns `400`). Once this calendar month's (UTC) AI cost reaches the cap, Ask, SQL generation, visualization, monitor-suggestion and dashboard explanation calls return `429` with code `ai_budget_exceeded` and a message like `Monthly AI budget of $5.00 reached; resets 2026-11-01.`. The check runs before any provider call, and the refused request is not metered. The cap is soft: spend is checked before each call and recorded after, so concurrent in-flight calls can overshoot it slightly. Monitor alerts still send, without AI analysis, and digest dashboard briefs are skipped. Calls on models with no known price have no cost and do not count toward the cap. |
| POST | `/api/organization/ai/test` | admin | Test an AI provider key without spending generation tokens. |
| GET | `/api/status` | admin | System health: Valkey queue depths, worker heartbeat. |
| GET | `/api/health` | none | Plain liveness check (for load balancers/orchestrators). |

---

Not covered above because they carry no request body/response shape worth documenting per-field
here (config-schema-driven forms): the `configuration_schema` returned by
`GET /api/data_sources/types` and `GET /api/destinations/types` is a JSON Schema-like object per
type — see [`connectors.md`](connectors.md) for the connector side of that.
