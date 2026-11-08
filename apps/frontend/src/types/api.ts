export interface User {
  id: number;
  name: string;
  email: string;
  // NotificationSettingsPage reads `digest_enabled` off this; narrowing it away breaks that read.
  [key: string]: unknown;
}

// Mirrors backend `object_type` strings.
export type ObjectType = "query" | "dashboard" | "monitor";

// Mirrors backend `visibility` column.
export type Visibility = "workspace" | "restricted" | "public";

// Mirrors backend `access_type`.
export type ObjectRole = "owner" | "editor" | "viewer";

/** Server-computed authz summary; bind UI to these, never recompute client-side. `can_share` exists
 * only on dashboards. */
export interface Caller {
  object_role: ObjectRole | "none";
  can_edit: boolean;
  can_delete: boolean;
  can_share?: boolean;
  /** Query-only: whether the caller can run this query (undefined on non-query objects). */
  can_execute?: boolean;
  execute_blocked_reason?: "capability" | "data_source" | "both";
}

export type ScopeFilter = "all" | "by-me" | "shared-with-me";

export interface UserDetails {
  id: number;
  org_id: number;
  name: string;
  email: string;
  groups: number[];
  is_invitation_pending: boolean;
  disabled: boolean;
  profile_image_url?: string | null;
  created_at: string;
  updated_at: string;
  digest_enabled: boolean;
  digest_hour: number | null;
  digest_timezone: string | null;
}

export interface Org {
  id: number;
  name: string;
  slug: string;
}

export interface ClientConfig {
  disable_public_urls: boolean;
  saml_enabled: boolean;
  ldap_enabled: boolean;
  /** False if no AI provider key is configured (org override or env default). */
  ai_sql_generation: boolean;
  /** AI monitor-suggestion availability; independent of `ai_sql_generation`. */
  ai_monitor_suggestions: boolean;
  /** Dashboard "Explain" is usable: the feature is available, the setting is on, and sample values
   * are included. */
  ai_dashboard_narratives?: boolean;
  /** False when the deployment replaces the built-in login; absent means enabled. */
  password_auth_enabled?: boolean;
}

export interface SessionResponse {
  user: User | null;
  org?: Org;
  client_config?: ClientConfig;
  permissions?: string[];
  is_admin?: boolean;
}

export interface DataSourceTypeField {
  type: "string" | "integer" | "number" | "boolean" | "file" | string;
  title?: string;
  description?: string;
  default?: unknown;
  enum?: unknown[];
  /** File-picker accept string (e.g. ".csv"), only meaningful when `type === "file"`. */
  accept?: string;
}

export interface UploadDataSourceResponse {
  upload_id: string;
  filename: string;
  byte_size: number;
  content_type: string;
  columns: ResultColumn[];
  preview_rows: Record<string, unknown>[];
  warning?: string;
}

/** JSON-Schema-ish; `secret` is a top-level array of field names to mask. Field order follows
 * `Object.keys(properties)`. */
export interface DataSourceConfigurationSchema {
  type?: string;
  properties?: Record<string, DataSourceTypeField>;
  required?: string[];
  secret?: string[];
}

export interface DataSourceType {
  type: string;
  name: string;
  configuration_schema: DataSourceConfigurationSchema;
}

/** Connectivity health, independent of `paused`; "unknown" = never used. */
export type DataSourceHealthState = "unknown" | "ok" | "failing";

export interface DataSource {
  id: number;
  name: string;
  type: string;
  /** Human-readable connector name; render this, never the raw `type` key. */
  type_display_name: string;
  schema?: DataSourceSchema | null;
  schema_refreshed_at: string | null;
  paused: boolean;
  pause_reason: string | null;
  health_state: DataSourceHealthState;
  health_error: string | null;
  health_checked_at: string | null;
  profiling?: boolean;
  profile_failed?: boolean;
  created_at: string;
  updated_at: string;
  options?: Record<string, unknown> | null;
  caller: Caller;
}

export interface DataSourceSchemaColumn {
  name: string;
  type_name?: string;
}

export interface DataSourceSchemaTable {
  name: string;
  columns: DataSourceSchemaColumn[];
}

export interface DataSourceSchema {
  tables: DataSourceSchemaTable[];
}

export interface DataSourceSchemaResponse {
  schema: DataSourceSchema;
  refreshing: boolean;
  refresh_error: string | null;
}

export interface TestConnectionResponse {
  message: string;
}

/** The legacy parameter types (mirrors backend `arcodash_core::models::ParameterType`). */
export type ParameterType =
  | "text"
  | "number"
  | "date"
  | "date-range"
  | "datetime-range"
  | "dropdown"
  | "query";

export interface MultiValuesOptions {
  prefix: string;
  suffix: string;
  separator: string;
}

export interface QueryParameterDefinition {
  name: string;
  title: string;
  type: ParameterType;
  value?: unknown;
  enum_options?: string;
  query_id?: number;
  multi_values_options?: MultiValuesOptions;
}

/** Runtime parameter value by name; a range needs both `start` and `end`. */
export type ParamValue = string | number | string[] | number[] | { start?: string; end?: string };

export interface QueryOptions {
  parameters?: QueryParameterDefinition[];
  max_age?: number;
}

export interface Query {
  id: number;
  data_source_id: number;
  name: string;
  description: string | null;
  query: string;
  query_hash: string;
  user_id: number;
  owner_name?: string | null;
  visibility: Visibility;
  latest_query_data_id: number | null;
  is_archived: boolean;
  /** True once published at least once; the sole source of truth for "is it live". */
  published: boolean;
  has_unpublished_changes: boolean;
  /** Draft-row concurrency token; send back as `expected_version` on
   * update/publish/discard/restore. */
  working_version: number;
  options: QueryOptions;
  tags: string[];
  is_favorite: boolean;
  last_execution_status: "New" | "Success" | "Failed";
  last_execution_at: string | null;
  row_count: number | null;
  created_at: string;
  updated_at: string;
  caller: Caller;
}

/** Mirrors backend `ScheduleSpec` (serde tag "kind"); `at.days` omitted/null = daily. */
export type ScheduleSpec =
  | { kind: "interval"; every_secs: number }
  | { kind: "cron"; expr: string }
  | { kind: "at"; time: string; days?: string[] | null };

export interface ScheduleLastRun {
  status: string;
  finished_at: string | null;
  duration_ms: number | null;
  error: string | null;
}

/** Mirrors backend `ScheduleResponse` (`GET/POST/PATCH /api/schedules`). */
export interface Schedule {
  id: number;
  entity_type: "query" | "monitor";
  entity_id: number;
  spec: ScheduleSpec;
  timezone: string;
  enabled: boolean;
  paused_until: string | null;
  auto_paused_at: string | null;
  overlap_policy: string;
  next_run_at: string | null;
  consecutive_failures: number;
  last_run: ScheduleLastRun | null;
}

export interface RunNotification {
  destination_name: string;
  status: string;
  error: string | null;
}

/** Mirrors backend `RunResponse`. `status` stays a `string` so unknown values can't break
 * rendering; branch via `lib/scheduling.ts`. */
export interface Run {
  id: number;
  status: string;
  triggered_by: string; // "schedule" | "manual"
  scheduled_for: string | null;
  started_at: string | null;
  finished_at: string | null;
  duration_ms: number | null;
  row_count: number | null;
  error: string | null;
  data_age_secs: number | null;
  waiting_on_run_id: number | null;
  query_result_id: number | null;
  /** Monitor runs only: whether the condition was met; `null` = never resolved, never read as
   * ok/alerting. */
  condition_met: boolean | null;
  notified: RunNotification[];
}

export interface QueryConsumers {
  tile_count: number;
  dashboard_count: number;
  monitor_count: number;
}

export interface ParameterValueOption {
  name: string;
  value: unknown;
}

/** Either the resolved option list or a `job` to poll (source query had no cached result). */
export type ParameterValuesResponse =
  | { values: ParameterValueOption[]; job?: undefined }
  | { job: Job; values?: undefined };

export type FriendlyType =
  | "string"
  | "integer"
  | "float"
  | "boolean"
  | "datetime"
  | "date";

export interface ResultColumn {
  name: string;
  friendly_type: FriendlyType | string;
}

export interface ResultData {
  columns: ResultColumn[];
  rows: Record<string, unknown>[];
  runtime_seconds?: number;
}

export interface QueryResult {
  id: number;
  data_source_id: number;
  query: string;
  query_hash: string;
  data: ResultData;
  runtime: number;
  retrieved_at: string;
}

export const JobStatus = {
  PENDING: 1,
  STARTED: 2,
  SUCCESS: 3,
  FAILURE: 4,
  CANCELLED: 5,
} as const;

export type JobStatusValue = (typeof JobStatus)[keyof typeof JobStatus];

export interface Job {
  id: string;
  status: JobStatusValue;
  query_result_id: number | null;
  error: string | null;
}

export type ExecuteResponse =
  | { query_result: QueryResult; job?: undefined }
  | { job: Job; query_result?: undefined };

/** Query snippet; `snippet` may contain TextMate `${1:placeholder}` tab stops, expanded via
 * CodeMirror. */
export interface Snippet {
  id: number;
  trigger: string;
  description: string | null;
  snippet: string;
  user_id: number;
  created_at: string;
  updated_at: string;
}

/** Editable-fields snapshot of a query as stored in `changes.change`. */
export interface QuerySnapshot {
  name: string;
  description: string | null;
  query: string;
  data_source_id: number;
  options: QueryOptions;
  is_archived: boolean;
}

/** One `changes` row with a server-computed 1-indexed version; oldest-first. */
export interface QueryVersion {
  version: number;
  change_id: number;
  user_id: number;
  user_name: string | null;
  change: QuerySnapshot;
  created_at: string;
}

export interface Visualization {
  id: number;
  query_id: number;
  type: string;
  name: string;
  description?: string;
  options: Record<string, unknown>;
  created_at: string;
  updated_at: string;
}

export interface WidgetPosition {
  col: number;
  row: number;
  sizeX: number;
  sizeY: number;
}

/** Tile freshness inputs. `retrieved_at` is the last-good result; `last_run_status` is the newest
 * run of any outcome, so a failed refresh can redden the dot without blanking the number. */
export interface WidgetFreshness {
  retrieved_at: string;
  /** Cadence proxy in seconds; null ⇒ query is unscheduled/disabled/paused (never amber). */
  expected_interval_secs: number | null;
  /** Newest finalized run status; a `string`, not a union — branch via `lib/scheduling.ts`. */
  last_run_status: string;
}

/** One monitor tile; a monitor missing from the array was deleted or unshared and renders as
 * unavailable. */
export interface MonitorTile {
  id: number;
  name: string;
  state: "ok" | "triggered" | "unevaluable" | "unknown";
  column: string | null;
  op: string | null;
  value: number | null;
  /** Any JSON value — render defensively. */
  observed: unknown;
  last_checked_at: string | null;
  muted: boolean;
  kind: MonitorKind;
  learning?: { have: number; need: number } | null;
  incident_summary?: string | null;
  check_failed: boolean;
  last_check_error?: string | null;
}

export interface Widget {
  id: number;
  dashboard_id: number;
  visualization_id: number | null;
  visualization?: Visualization & {
    query?: {
      id: number;
      name: string;
      data_source_id: number;
      /** Parameter tokens referenced by the served SQL — the SQL text itself is never sent here. */
      parameter_names: string[];
      options: QueryOptions;
    };
  };
  /** Pre-loaded cached result data — only present on public dashboard responses. */
  query_result_data?: ResultData | null;
  freshness?: WidgetFreshness | null;
  /** Only for monitor-bound widgets. */
  monitors?: MonitorTile[] | null;
  /** Whether the viewer can read the underlying query results; `true` for text widgets. */
  viewer_can_read_data: boolean;
  text: string | null;
  width: number;
  /** `visualization_id != null` ⇒ viz widget; else `options.monitors?.length` ⇒ monitor widget;
   * else text widget. */
  options: { position?: WidgetPosition; monitors?: number[] };
  created_at: string;
  updated_at: string;
}

export interface ChangeSummary {
  added: number;
  removed: number;
  moved: number;
  resized: number;
  settings_changed: number;
}

export interface Dashboard {
  id: number;
  slug: string;
  name: string;
  user_id: number;
  owner_name?: string | null;
  visibility: Visibility;
  layout: unknown;
  dashboard_filters_enabled: boolean;
  is_archived: boolean;
  /** True once published at least once; the sole source of truth for "is it live". */
  published: boolean;
  has_unpublished_changes: boolean;
  /** Populated only for an editor viewing a dashboard with unpublished changes. */
  pending_change_summary?: ChangeSummary;
  tags: string[];
  options: unknown;
  is_favorite: boolean;
  widgets: Widget[];
  created_at: string;
  updated_at: string;
  caller: Caller;
}

export interface DashboardListItem {
  id: number;
  slug: string;
  name: string;
  user_id: number;
  owner_name?: string | null;
  visibility: Visibility;
  is_archived: boolean;
  published: boolean;
  has_unpublished_changes: boolean;
  tags: string[];
  is_favorite: boolean;
  created_at: string;
  updated_at: string;
  caller: Caller;
}

export type MonitorKind = "threshold" | "baseline";
export type Sensitivity = "low" | "medium" | "high";

export interface MonitorObservation {
  observed_at: string;
  value: number | null;
  state: string;
  explanation: string | null;
  run_id: number | null;
}

export interface MonitorOptions {
  column: string;
  op?: string | null;
  value?: number | null;
  kind?: MonitorKind;
  sensitivity?: Sensitivity | null;
  muted?: boolean;
  /** RFC3339; present while snoozed. Silenced (no notify/escalate) until this timestamp passes. */
  muted_until?: string | null;
  /** Custom email subject; `{{name}}`/`{{state}}` are substituted. */
  custom_subject?: string;
  /** Custom email body; `{{name}}`/`{{state}}` are substituted. */
  custom_body?: string;
  /** Max seconds a check waits for an in-flight refresh before using the last-known result; null =
   * always wait. */
  max_fresh_wait_secs?: number | null;
}

export interface QuerySummary {
  id: number;
  name: string;
}

export interface UserSummary {
  id: number;
  name: string;
  email: string;
}

export interface EscalationStep {
  delay_minutes: number;
  destination_id: number;
}

export interface Monitor {
  id: number;
  name: string;
  query: QuerySummary;
  options: MonitorOptions;
  state: string;
  last_triggered_at?: string | null;
  rearm?: number | null;
  user: UserSummary;
  org_id: number;
  visibility: Visibility;
  created_at: string;
  updated_at: string;
  acked_at: string | null;
  /** Server-resolved acker email, NOT the raw `acked_by` id. */
  acked_by_email: string | null;
  kind: MonitorKind;
  learning?: { have: number; need: number } | null;
  /** 0 = no escalation step has fired; k = steps 1..k have fired. */
  escalation_step: number;
  /** Set only when `state === "unevaluable"`: why the check couldn't resolve. */
  unevaluable_reason: string | null;
  escalation: EscalationStep[];
  caller: Caller;
}

export interface MonitorSubscription {
  id: number;
  monitor_id: number;
  user: UserSummary;
  destination?: { id: number; name: string; type: string } | null;
}

export interface MonitorDelivery {
  id: number;
  created_at: string;
  destination_id: number | null;
  subscription_id: number | null;
  state: string;
  status: "sent" | "failed" | "retrying";
  error: string | null; // redacted, safe to display
  attempt_number: number;
}

export interface Destination {
  id: number;
  name: string;
  type: string;
  options: Record<string, unknown>;
  icon: string;
  created_at: string;
  /** Redacted display of the target — never a raw secret. */
  target: string | null;
}

export interface DestinationType {
  type: string;
  name: string;
  icon: string;
  configuration_schema: Record<string, { type: string; title: string }>;
  secret: string[];
  required: string[];
}

export interface AccessPermission {
  id: number;
  object_type: string;
  object_id: number;
  access_type: ObjectRole;
  grantor_id: number;
  grantee_id: number | null;
  grantee_group_id: number | null;
  created_at: string;
}

export interface ShareInfo {
  secret_token: string;
  public_url: string;
}

export type EmbedObjectType = "dashboard" | "visualization";

/** One `embeds` row from the authenticated CRUD; anonymous viewers get `PublicEmbedResponse`. */
export interface Embed {
  id: number;
  object_type: EmbedObjectType;
  object_id: number;
  mode: "public" | "signed";
  token: string | null;
  public_url: string | null;
  locked_params: Record<string, unknown>;
  expires_at: string | null;
  anon_authorized: boolean;
  revoked_at: string | null;
  created_at: string;
}

export interface CreateEmbedInput {
  object_type: EmbedObjectType;
  object_id: number;
  anonymous?: boolean;
  locked_params?: Record<string, unknown>;
  expires_at?: string | null;
}

export interface OrgEmbedSettings {
  public_urls_enabled: boolean;
}

export interface PublicViz {
  type: string;
  name: string;
  options: Record<string, unknown>;
  data: unknown;
}

export interface PublicWidget {
  id: number;
  text: string | null;
  width: number;
  position?: WidgetPosition | null;
  visualization: PublicViz | null;
  freshness?: WidgetFreshness | null;
  monitors?: MonitorTile[] | null;
}

export interface PublicEmbedResponse {
  object_type: EmbedObjectType;
  title: string;
  widgets: PublicWidget[];
  visualization: PublicViz | null;
  as_of: string | null;
}

export interface Group {
  id: number;
  name: string;
  type: string;
  created_at: string;
  updated_at: string;
}

export interface GroupMember {
  id: number;
  name: string;
  email: string;
}

export interface GroupDataSource {
  id: number;
  data_source_id: number;
  group_id: number;
  name: string;
}

export interface GroupDataSourceAccess {
  restricted: boolean;
  entries: GroupDataSource[];
}

/** Reverse of `GroupDataSourceAccess`; `has_access` already folds in the unrestricted case. */
export interface DataSourceGroupAccess {
  group_id: number;
  group_name: string;
  restricted: boolean;
  has_access: boolean;
}

/** `can_create` reflects the union rule across the caller's groups. */
export interface ListDataSourcesResponse {
  data_sources: DataSource[];
  can_create: boolean;
}

export interface Role {
  id: number;
  name: string;
  type: string; // "builtin" | "custom"
  permissions: string[];
  group_count: number;
  created_at: string;
  updated_at: string;
}
export interface Permission { key: string; label: string; category: string; }
export interface GroupRole { id: number; name: string; permissions: string[]; }
export interface EffectivePermissions { permissions: string[]; is_admin: boolean; }

export interface OrgSettings {
  id: number;
  name: string;
  slug: string;
  settings: Record<string, unknown>;
  created_at: string;
  updated_at: string;
}

/** A resolved object reference; `path` only when the object has a real per-id page. */
export interface AuditEventObjectRef {
  name: string;
  path: string | null;
}

export interface AuditEvent {
  id: number;
  org_id: number;
  user_id: number | null;
  user_name: string | null;
  action: string;
  object_type: string | null;
  object_id: number | null;
  /** Null when the object was deleted or has no name field. */
  object: AuditEventObjectRef | null;
  additional_properties: Record<string, unknown>;
  created_at: string;
}

export interface SystemStatus {
  status: string;
  version: string;
  jobs: {
    queued: number;
    in_flight: number;
    recent_success: number;
    recent_failure: number;
    recent_cancelled: number;
  };
  /** Fleet liveness (global, not per-org); count only, never per-worker identity. */
  workers: SystemStatusWorkers;
}

export interface SystemStatusWorkers {
  online: number;
}

export interface AiProviderInfo {
  key_configured: boolean;
  key_source: "org" | "env" | "none";
  model: string | null;
  /** Org-configured endpoint override — always `null` for providers other than OpenAI. */
  base_url: string | null;
  default_model: string;
  curated_models: string[];
  last_status: "ok" | "invalid_key" | "quota_exceeded" | "error" | "unavailable" | null;
  last_status_at: string | null;
  free_tier_seen_at?: string | null;
}

export interface AiSettings {
  key_configured: boolean;
  key_source: "org" | "env" | "none";
  rate_limit_org_value: number | null;
  rate_limit_env_default: number;
  rate_limit_effective: number;
  /** `[0]` is the active/primary provider; `[]` if none configured. */
  provider_order: string[];
  fallback_enabled: boolean;
  include_sample_values: boolean;
  explain_alerts?: boolean;
  dashboard_narratives?: boolean;
  /** Monthly spend cap in USD; `null` means no cap. */
  monthly_budget_usd: number | null;
  providers: Record<string, AiProviderInfo>;
}

export interface AiProfileColumn {
  name: string;
  null_frac: number;
  distinct_in_sample: number;
  samples: string[];
  min?: string | null;
  max?: string | null;
  description?: string | null;
}

export interface AiProfileTable {
  name: string;
  sampled_rows: number;
  columns: AiProfileColumn[];
  error?: string | null;
  data_profiled_at?: string | null;
}

export interface AiRelationship {
  from_table: string;
  from_column: string;
  to_table: string;
  to_column: string;
  source: "fk" | "mined" | "name";
}

export interface DataSourceAiProfile {
  tables: AiProfileTable[];
  relationships: AiRelationship[];
  schema_refreshed_at_seen: string;
  descriptions_used_samples: boolean;
}

export interface DataSourceAiProfileResponse {
  profile: DataSourceAiProfile | null;
  built_at: string | null;
  supported: boolean;
  building: boolean;
  current: boolean;
  phase?: "schema" | "data" | null;
  last_error?: string | null;
  /** Source bills per query, so sample values refresh only on an explicit rebuild. */
  metered?: boolean;
}

export interface TestKeyResult {
  status: "ok" | "invalid_key" | "quota_exceeded" | "network_error";
  message: string;
}

interface AiUsageBreakdown {
  calls: number;
  tokens_in: number;
  tokens_out: number;
  error_rate: number;
  cost_usd: number | null;
}

export interface AiUsageResponse {
  days: number;
  totals: {
    total_calls: number;
    tokens_in: number;
    tokens_out: number;
    /** Fraction (0.0–1.0) of calls with `status === "ok"`. */
    success_rate: number;
    cost_usd: number | null;
  };
  by_provider: (AiUsageBreakdown & { provider: string })[];
  by_feature: (AiUsageBreakdown & { feature: string })[];
  by_day: {
    /** `YYYY-MM-DD`, UTC. */
    date: string;
    status: string;
    count: number;
    cost_usd: number | null;
  }[];
  by_user: AiUsageByUser[];
  budget: AiUsageBudget;
}

export interface AiUsageByUser {
  user_id: number | null;
  email: string | null;
  name: string | null;
  calls: number;
  tokens_in: number;
  tokens_out: number;
  cost_usd: number | null;
}

export interface AiUsageBudget {
  monthly_usd: number | null;
  spent_month_usd: number;
  /** RFC 3339 instant the monthly window next resets (1st of the month, UTC). */
  resets_at: string;
}

/** One SSE event from `POST /api/queries/ask`, discriminated by `type`. */
export type AskEvent =
  | { type: "status"; text: string }
  | { type: "draft"; attempt: number; sql: string; explanation?: string | null }
  | { type: "executing"; attempt: number }
  | {
      type: "preview";
      attempt: number;
      columns: { name: string; friendly_type: string }[];
      rows: unknown[];
      row_count: number;
      truncated: boolean;
    }
  | { type: "repair"; attempt: number; error: string }
  | { type: "profile_refreshing"; data_source_id: number }
  | {
      type: "final";
      sql: string;
      explanation?: string | null;
      served_by: string;
      attempts: number;
      tokens_in: number;
      tokens_out: number;
      executed?: boolean;
      error?: string | null;
    }
  | { type: "visualizing" }
  | { type: "suggesting" }
  | { type: "suggested"; sql: string; data_source_id: number; suggestion: QueryMonitorSuggestion }
  | {
      type: "created";
      query_id: number;
      visualization_id: number;
      widget_id: number;
      viz_type: string;
      row_count: number;
    }
  | { type: "failed"; status: string; message: string; retry_after_secs?: number };

/** Failure variants shared by every AI-backed endpoint. */
export type AiErrorResponse = {
  status: string;
  message: string;
  retry_after_secs?: number;
};

/** The failure statuses of the suggest endpoints. */
type AiFailure =
  | { status: "not_configured" | "invalid_key" | "quota_exceeded" | "generation_failed"; message: string }
  | { status: "rate_limited"; message: string; retry_after_secs: number };

export type SuggestVisualizationResponse =
  | {
      status: "ok";
      title: string;
      type: string;
      options: Record<string, unknown>;
      served_by: string;
    }
  | AiFailure;

/** AI-proposed check. `kind` is a display label only (never sent to `POST /api/monitors`); `column`
 * is read-only. */
export interface MonitorSuggestion {
  table: string;
  kind: "freshness" | "null_rate" | "uniqueness" | "row_count";
  title: string;
  rationale: string;
  sql: string;
  column: string;
  target_column: string;
  op: string;
  value: number;
  guessed_threshold: boolean;
}

/** A table whose only sensible check already exists; rendered as "Already covered". */
export interface CoveredTable {
  table: string;
  monitor_id: number;
  monitor_name: string;
}

/** Tagged union mirroring the ask failure variants, so `aiErrorMessage` works unchanged. */
export type SuggestMonitorsResponse =
  | {
      status: "ok";
      suggestions: MonitorSuggestion[];
      covered: CoveredTable[];
      scanned_tables: number;
      served_by: string;
    }
  | AiFailure;

export interface QueryMonitorSuggestion {
  name: string;
  kind: MonitorKind;
  column: string;
  sensitivity: Sensitivity | null;
  op: string | null;
  value: number | null;
  definition: string;
  why: string;
  confidence: Sensitivity;
}

export interface CoveredColumn {
  column: string;
  monitor_id: number;
  monitor_name: string;
  kind: MonitorKind;
}

export type SuggestQueryMonitorsResponse =
  | {
      status: "ok";
      suggestions: QueryMonitorSuggestion[];
      already_covered: CoveredColumn[];
      unsupported: string | null;
      served_by: string;
    }
  | AiFailure;

export interface DashboardNarrative {
  id: number;
  dashboard_id: number;
  created_at: string;
  source: "manual" | "digest";
  summary: string;
  /** Markdown; widget links are `[title](#widget-<id>)`. Null on legacy rows. */
  body: string | null;
  highlights: { widget_id: number; text: string }[];
  confidence: "low" | "medium" | "high";
  model: string;
}

export type ExplainDashboardResponse =
  | { status: "ok"; narrative: DashboardNarrative }
  | { status: "disabled" | "not_configured" | "invalid_key" | "quota_exceeded" | "generation_failed"; message: string }
  | { status: "rate_limited"; message: string; retry_after_secs: number };
