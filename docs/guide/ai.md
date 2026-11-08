# AI features

All AI features use your own provider key: Anthropic, OpenAI or Gemini. Set an instance-wide key
with `ANTHROPIC_API_KEY`, `OPENAI_API_KEY` or `GEMINI_API_KEY` (see
[ENV_VARS.md](../../ENV_VARS.md#ai-bring-your-own-key)), or let an organization admin add one in
**AI Settings** (`/organization/ai`; the **Ask with AI** tab links there until a key is set), which takes precedence.

## What the AI sees

Prompts contain the data source's schema, a few sample values per column (from its AI profile),
your SQL and, when repairing, the error message. Query results and preview rows are never sent,
except for the alert explanations and dashboard summaries below, which only run while **Include
sample values** (`include_sample_values`, default on) is enabled. With it off, column stats are
still used but samples and min/max values are left out.

## Features

- **Ask with AI.** In the query editor, describe what you want: the AI drafts SQL, runs a
  read-only preview and repairs it if it errors. **Fix with AI** repairs a failing query.
- **Add with AI on dashboards.** In edit mode, describe a chart ("signups by country, last 30
  days"). A saved query tagged `ai-widget` and a tile appear in your draft; publish to show it.
  Removing an untouched AI tile also removes its query.
- **Add with AI on monitors.** Describe what to watch ("alert me when daily signups drop"). The
  AI writes and runs the query and pre-fills the monitor form; nothing is saved until you click
  **Save**. **Suggest with AI** in the query editor proposes monitors for an existing query.
- **Explain a dashboard.** A short summary of what the widgets show and how they moved since the
  last summary, from their cached results (no query is re-run). Favorite dashboards also get a
  brief in the daily digest. Needs **Explain dashboards with AI** (`dashboard_narratives`).
- **Why did this fire?** Every alert carries a stats line. With **Explain alerts**
  (`explain_alerts`) on, up to 3 read-only aggregate queries run on the monitor's data source and
  the aggregated rows go to the provider for a short analysis. An explanation never delays an
  alert by more than 20 seconds.

These features need the same permissions as doing the work by hand (creating queries, monitors).

## Budget and usage

Set a **Monthly budget** (USD) in AI Settings to cap spend. Cost is estimated from list prices
per model (unknown models are not priced) and shown on the **AI Usage** page, per user. When the
month's spend (UTC) reaches the cap, AI features pause until the 1st of the next month; calls
already in flight can overshoot slightly. Alerts keep sending without the AI analysis.

## The AI profile

A background profile feeds the prompts. The schema pass reads metadata only (tables, columns,
relationships); the data pass reads up to 200 rows per table for sample values and stats. Views
are never sampled, and AI-written column descriptions survive rebuilds for unchanged columns.

- Manual schema refresh: schema pass, plus the data pass for new or changed tables.
- Rebuild (admin): both passes, all tables.
- A "table/column does not exist" error during Ask or Fix triggers a refresh (at most once per
  10 minutes per data source) and one retry.
- Daily: schema pass only, for organizations that used AI in the last 30 days.
- Metered sources (BigQuery, Athena, Snowflake, Redshift) are sampled only on Rebuild and skipped
  by the daily pass.

Failures show on the data sources list and the AI profile card.
