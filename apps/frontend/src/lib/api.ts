/** Typed fetch client; auth is the httpOnly session cookie. Base URL comes from `VITE_API_BASE_URL` — never hardcode a deployment URL. */
import type {
  AccessPermission,
  AiSettings,
  DashboardNarrative,
  ExplainDashboardResponse,
  AiUsageResponse,
  EscalationStep,
  Monitor,
  MonitorDelivery,
  MonitorObservation,
  MonitorOptions,
  MonitorSubscription,
  AuditEvent,
  Dashboard,
  DashboardListItem,
  DataSource,
  DataSourceGroupAccess,
  DataSourceAiProfileResponse,
  DataSourceSchemaResponse,
  DataSourceType,
  CreateEmbedInput,
  Destination,
  DestinationType,
  Embed,
  ExecuteResponse,
  EffectivePermissions,
  AskEvent,
  Group,
  GroupDataSourceAccess,
  GroupMember,
  GroupRole,
  Job,
  ListDataSourcesResponse,
  ObjectRole,
  ObjectType,
  OrgEmbedSettings,
  OrgSettings,
  ParameterValuesResponse,
  Permission,
  PublicEmbedResponse,
  Query,
  QueryConsumers,
  QueryOptions,
  QueryResult,
  QueryVersion,
  Role,
  Run,
  Schedule,
  ScheduleSpec,
  ScopeFilter,
  SessionResponse,
  ShareInfo,
  Snippet,
  SuggestMonitorsResponse,
  SuggestQueryMonitorsResponse,
  SuggestVisualizationResponse,
  SystemStatus,
  TestConnectionResponse,
  TestKeyResult,
  UploadDataSourceResponse,
  User,
  UserDetails,
  Visibility,
  Visualization,
  Widget,
  WidgetPosition,
} from "@/types/api";
import { notifySessionExpired } from "@/lib/sessionExpiry";
import { readSse } from "@/lib/sse";

const OBJECT_TYPE_PATH: Record<ObjectType, string> = {
  query: "queries",
  dashboard: "dashboards",
  monitor: "monitors",
};

const API_BASE_URL = import.meta.env.VITE_API_BASE_URL ?? "";

const ASK_EVENTS = new Set(["status", "draft", "executing", "preview", "repair", "profile_refreshing", "final", "visualizing", "suggesting", "suggested", "created", "failed"]);

async function streamAsk(
  path: string,
  body: unknown,
  onEvent: (event: AskEvent) => void,
  signal?: AbortSignal,
): Promise<void> {
  const csrfToken = getCsrfToken();
  const res = await fetch(`${API_BASE_URL}${path}`, {
    method: "POST",
    credentials: "include",
    signal,
    body: JSON.stringify(body),
    headers: {
      Accept: "text/event-stream",
      "Content-Type": "application/json",
      ...(csrfToken ? { "X-CSRF-Token": csrfToken } : {}),
    },
  });
  if (res.status === 401) notifySessionExpired();
  if (!res.ok || !res.body) return handleResponse<void>(res);
  await readSse(res.body, ({ event, data }) => {
    if (!ASK_EVENTS.has(event)) return;
    try {
      onEvent({ type: event, ...JSON.parse(data) } as AskEvent);
    } catch {
      // malformed payload: drop the event, keep the stream
    }
  });
}

export class ApiError extends Error {
  status: number;
  errorCode?: string;

  constructor(status: number, message: string, errorCode?: string) {
    super(message);
    this.name = "ApiError";
    this.status = status;
    this.errorCode = errorCode;
  }
}

/** A stale `expected_version` 409; carries the row's current `working_version` for reconciling. */
export class ConflictError extends ApiError {
  workingVersion: number;

  constructor(message: string, workingVersion: number) {
    super(409, message, "conflict");
    this.name = "ConflictError";
    this.workingVersion = workingVersion;
  }
}

export function apiErrorMessage(err: unknown, fallback = "Something went wrong"): string {
  return err instanceof ApiError ? err.message : fallback;
}

function getCsrfToken(): string | null {
  return document.cookie
    .split(";")
    .map((c) => c.trim())
    .find((c) => c.startsWith("csrf_token="))
    ?.split("=")[1] ?? null;
}

/** Aborts (not just abandons) the request at the timeout: a late autosave PATCH must not bump the row's version after the caller hit the failure path.
 * A caller-supplied signal owns its own cancellation. */
const DEFAULT_TIMEOUT_MS = 60_000;

function withTimeout<T>(promise: Promise<T>, ms: number, controller?: AbortController): Promise<T> {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      controller?.abort();
      reject(new ApiError(408, "The request timed out"));
    }, ms);
    promise.then(
      (value) => {
        clearTimeout(timer);
        resolve(value);
      },
      (err) => {
        clearTimeout(timer);
        reject(err);
      },
    );
  });
}

async function handleResponse<T>(res: Response): Promise<T> {
  if (!res.ok) {
    let message = res.statusText;
    let body: { message?: string; error_code?: string; working_version?: number } | null = null;
    try {
      body = await res.json();
      if (typeof body?.message === "string") {
        message = body.message;
      }
    } catch {
    }
    if (res.status === 409 && body?.error_code === "conflict" && typeof body.working_version === "number") {
      throw new ConflictError(message, body.working_version);
    }
    throw new ApiError(res.status, message, body?.error_code);
  }

  if (res.status === 204) {
    return undefined as T;
  }

  const text = await res.text();
  if (!text) return undefined as T;
  try {
    return JSON.parse(text) as T;
  } catch {
    throw new ApiError(res.status, "Unexpected non-JSON response from the server");
  }
}

/** `/api/session` must never trigger the 401 handler: a signed-out probe legitimately 401s and
 * would risk a redirect loop. */
const SESSION_PROBE_PATH = "/api/session";

export interface ApiFetchInit extends RequestInit {
  /** Skip the 401-clears-the-session handler, for credential checks whose 401 means wrong
   * credentials. */
  skipSessionExpiry?: boolean;
  /** Return the body as a `Blob` (CSV/TSV/XLSX download); non-2xx is still parsed as the JSON error
   * body. */
  responseType?: "json" | "blob";
}

export async function apiFetch<T>(path: string, init?: ApiFetchInit): Promise<T> {
  const { skipSessionExpiry, responseType, ...requestInit } = init ?? {};
  const method = (requestInit.method ?? "GET").toUpperCase();
  const isMutating = !["GET", "HEAD", "OPTIONS", "TRACE"].includes(method);
  const csrfToken = isMutating ? getCsrfToken() : null;
  // A caller-supplied signal owns cancellation; otherwise create our own controller so the timeout
  // truly aborts.
  const controller = requestInit.signal ? undefined : new AbortController();
  const rawFetch = fetch(`${API_BASE_URL}${path}`, {
    ...requestInit,
    credentials: "include",
    signal: requestInit.signal ?? controller?.signal,
    headers: {
      Accept: "application/json",
      ...(requestInit.body ? { "Content-Type": "application/json" } : {}),
      ...(csrfToken ? { "X-CSRF-Token": csrfToken } : {}),
      ...(requestInit.headers ?? {}),
    },
  });
  const res = requestInit.signal
    ? await rawFetch
    : await withTimeout(rawFetch, DEFAULT_TIMEOUT_MS, controller);
  // Single choke point for 401s, including callers outside react-query that never reach the
  // handlers in `lib/sessionExpiry.ts`.
  if (res.status === 401 && !skipSessionExpiry && path !== SESSION_PROBE_PATH) {
    notifySessionExpired();
  }
  if (responseType === "blob" && res.ok) {
    return res.blob() as unknown as Promise<T>;
  }
  return handleResponse<T>(res);
}

/** Like `apiFetch`, but also reads the `X-Total-Count` header (body stays a plain array). */
async function apiFetchWithTotal<T>(path: string): Promise<{ data: T; total: number }> {
  const controller = new AbortController();
  const rawFetch = fetch(`${API_BASE_URL}${path}`, {
    credentials: "include",
    signal: controller.signal,
    headers: { Accept: "application/json" },
  });
  const res = await withTimeout(rawFetch, DEFAULT_TIMEOUT_MS, controller);
  const total = Number(res.headers.get("x-total-count") ?? 0);
  const data = await handleResponse<T>(res);
  return { data, total: Number.isFinite(total) ? total : 0 };
}

/** Consumes `window.__SESSION_PRELOAD__` once (safe under StrictMode); only a missing `Response` falls back to a fetch, an error status propagates.
 * The preload started before this module loaded, so a timeout can only stop waiting, not cancel. */
function getSession(): Promise<SessionResponse> {
  const preload = window.__SESSION_PRELOAD__;
  if (!preload) {
    return apiFetch<SessionResponse>("/api/session");
  }
  delete window.__SESSION_PRELOAD__;

  return withTimeout(preload, DEFAULT_TIMEOUT_MS)
    .then((res) => handleResponse<SessionResponse>(res))
    .catch((err) => {
      if (err instanceof ApiError) throw err;
      return apiFetch<SessionResponse>("/api/session");
    });
}

function sendJson<T>(method: string, path: string, body?: unknown, init?: ApiFetchInit): Promise<T> {
  return apiFetch<T>(path, {
    ...init,
    method,
    body: body === undefined ? undefined : JSON.stringify(body),
  });
}

const postJson = <T>(path: string, body?: unknown, init?: ApiFetchInit) =>
  sendJson<T>("POST", path, body, init);
const putJson = <T>(path: string, body?: unknown) => sendJson<T>("PUT", path, body);
const patchJson = <T>(path: string, body?: unknown) => sendJson<T>("PATCH", path, body);
const deleteJson = <T>(path: string) => sendJson<T>("DELETE", path);

/** `?k=v&...` from the truthy entries, or "" when there are none. */
function queryString(params: Record<string, string | number | boolean | undefined>): string {
  const qs = new URLSearchParams(
    Object.entries(params).flatMap(([k, v]) => (v ? [[k, String(v)]] : [])),
  ).toString();
  return qs ? `?${qs}` : "";
}

/** `fetch` has no upload-progress event, so uploads use `XMLHttpRequest`; base URL, cookie auth and
 * CSRF mirror `apiFetch`. */
function uploadDataSourceFile(
  input: { type: string; file: File; delimiter?: string },
  onProgress?: (fraction: number) => void,
): Promise<UploadDataSourceResponse> {
  return new Promise((resolve, reject) => {
    const xhr = new XMLHttpRequest();
    xhr.open("POST", `${API_BASE_URL}/api/data_sources/uploads`);
    xhr.withCredentials = true;
    const csrfToken = getCsrfToken();
    if (csrfToken) xhr.setRequestHeader("X-CSRF-Token", csrfToken);

    xhr.upload.onprogress = (e) => {
      if (onProgress && e.lengthComputable) onProgress(e.loaded / e.total);
    };

    xhr.onload = () => {
      let body: { message?: string; error_code?: string } | null = null;
      try {
        body = xhr.responseText ? JSON.parse(xhr.responseText) : null;
      } catch {
      }
      if (xhr.status >= 200 && xhr.status < 300) {
        resolve(body as unknown as UploadDataSourceResponse);
      } else {
        reject(new ApiError(xhr.status, body?.message ?? xhr.statusText ?? "Upload failed", body?.error_code));
      }
    };
    xhr.onerror = () => reject(new ApiError(0, "Network error during upload"));

    const form = new FormData();
    form.append("type", input.type);
    if (input.delimiter !== undefined) form.append("delimiter", input.delimiter);
    form.append("file", input.file, input.file.name);
    xhr.send(form);
  });
}

export const api = {
  // Wrong credentials, not an expired session — skip the global 401 handler.
  login: (email: string, password: string) =>
    postJson<User>("/api/login", { email, password }, { skipSessionExpiry: true }),

  logout: () => postJson<void>("/api/logout"),

  getSession,

  // Google OAuth and SAML are full-page redirects, not fetch() calls.
  samlLoginUrl: () => `${API_BASE_URL}/api/saml/login`,

  ldapLogin: (username: string, password: string) =>
    postJson<User>("/api/ldap/login", { username, password }, { skipSessionExpiry: true }),

  listDataSourceTypes: () => apiFetch<DataSourceType[]>("/api/data_sources/types"),

  listDataSources: () => apiFetch<ListDataSourcesResponse>("/api/data_sources"),

  getDataSource: (id: number) => apiFetch<DataSource>(`/api/data_sources/${id}`),

  createDataSource: (body: { name: string; type: string; options: Record<string, unknown> }) =>
    postJson<DataSource>("/api/data_sources", body),

  updateDataSource: (
    id: number,
    body: Partial<{ name: string; type: string; options: Record<string, unknown> }>,
  ) => postJson<DataSource>(`/api/data_sources/${id}`, body),

  testDataSource: (
    body: { type: string; options: Record<string, unknown> } | { id: number },
  ) => postJson<TestConnectionResponse>("/api/data_sources/test", body),

  uploadDataSourceFile,

  getDataSourceSchema: (id: number, refresh = false) =>
    apiFetch<DataSourceSchemaResponse>(
      `/api/data_sources/${id}/schema?refresh=${refresh ? "true" : "false"}`,
    ),

  getDataSourceAiProfile: (id: number) =>
    apiFetch<DataSourceAiProfileResponse>(`/api/data_sources/${id}/ai_profile`),

  /** A 409 means a build is already queued; it resolves as a value, not an error. */
  rebuildDataSourceAiProfile: async (id: number): Promise<{ status: "queued" | "already_queued" }> => {
    try {
      return await postJson<{ status: "queued" }>(`/api/data_sources/${id}/ai_profile/rebuild`);
    } catch (err) {
      if (err instanceof ApiError && err.status === 409) return { status: "already_queued" };
      throw err;
    }
  },

  pauseDataSource: (id: number, reason?: string) =>
    postJson<DataSource>(`/api/data_sources/${id}/pause`, reason ? { reason } : undefined),

  resumeDataSource: (id: number) => postJson<DataSource>(`/api/data_sources/${id}/resume`),

  deleteDataSource: (id: number) => deleteJson<void>(`/api/data_sources/${id}`),

  listDataSourceGroups: (id: number) =>
    apiFetch<DataSourceGroupAccess[]>(`/api/data_sources/${id}/groups`),

  listQueries: (includeArchived = false, q?: string, filter?: ScopeFilter) =>
    apiFetch<Query[]>(
      `/api/queries?include_archived=${includeArchived ? "true" : "false"}${q ? `&q=${encodeURIComponent(q)}` : ""}${filter ? `&filter=${filter}` : ""}`,
    ),

  createQuery: (body: {
    data_source_id: number;
    name: string;
    description?: string;
    query: string;
    options?: QueryOptions;
    tags?: string[];
  }) => postJson<Query>("/api/queries", body),

  getQuery: (id: number) => apiFetch<Query>(`/api/queries/${id}`),

  getQueryTags: () => apiFetch<{ tags: string[] }>("/api/queries/tags"),

  updateQuery: (
    id: number,
    body: Partial<{
      data_source_id: number;
      name: string;
      description: string;
      query: string;
      options: QueryOptions;
      is_archived: boolean;
      /** Must match the row's current `working_version` or the update 409s. */
      expected_version: number;
      tags: string[];
    }>,
  ) => postJson<Query>(`/api/queries/${id}`, body),

  publishQuery: (id: number, expectedVersion: number) =>
    postJson<Query>(`/api/queries/${id}/publish`, { expected_version: expectedVersion }),

  discardQueryChanges: (id: number, expectedVersion: number) =>
    postJson<Query>(`/api/queries/${id}/discard`, { expected_version: expectedVersion }),

  unpublishQuery: (id: number, expectedVersion: number) =>
    postJson<Query>(`/api/queries/${id}/unpublish`, { expected_version: expectedVersion }),

  verifyQuery: (id: number) => postJson<Query>(`/api/queries/${id}/verify`, {}),

  unverifyQuery: (id: number) => deleteJson<Query>(`/api/queries/${id}/verify`),

  restoreQueryVersion: (id: number, changeId: number, expectedVersion: number) =>
    postJson<Query>(`/api/queries/${id}/restore`, {
      change_id: changeId,
      expected_version: expectedVersion,
    }),

  refreshQuery: (id: number) => postJson<ExecuteResponse>(`/api/queries/${id}/refresh`),

  deleteQuery: (id: number) => deleteJson<void>(`/api/queries/${id}`),

  executeQuery: (id: number, body?: { parameters?: Record<string, unknown>; max_age?: number }) =>
    postJson<ExecuteResponse>(`/api/queries/${id}/results`, body ?? {}),

  // publishedOnly: a widget must show the published result, else the tile leaks the editor's draft
  // test run.
  getLatestQueryResult: (id: number, publishedOnly?: boolean) =>
    apiFetch<{ query_result: QueryResult }>(
      `/api/queries/${id}/results${publishedOnly ? "?published_only=true" : ""}`,
    ),

  getParameterValues: (queryId: number, parameterName: string) =>
    apiFetch<ParameterValuesResponse>(
      `/api/queries/${queryId}/parameter_values?name=${encodeURIComponent(parameterName)}`,
    ),

  listSchedules: (entity?: { entity_type: "query" | "monitor"; entity_id: number }) =>
    apiFetch<Schedule[]>(
      entity
        ? `/api/schedules?entity_type=${entity.entity_type}&entity_id=${entity.entity_id}`
        : "/api/schedules",
    ),

  /** 0-or-1 row for an entity (the collection is UNIQUE per entity) — unwrap to a single value. */
  getSchedule: (entityType: "query" | "monitor", entityId: number) =>
    apiFetch<Schedule[]>(`/api/schedules?entity_type=${entityType}&entity_id=${entityId}`).then(
      (rows) => rows[0] ?? null,
    ),

  createSchedule: (body: {
    entity_type: "query" | "monitor";
    entity_id: number;
    spec: ScheduleSpec;
    timezone?: string;
    overlap_policy?: "skip" | "coalesce";
    enabled?: boolean;
  }) => postJson<Schedule>("/api/schedules", body),

  updateSchedule: (
    id: number,
    body: Partial<{
      spec: ScheduleSpec;
      timezone: string;
      overlap_policy: "skip" | "coalesce";
      enabled: boolean;
      /** `null` clears paused_until (resume); an ISO string sets it. Omit to leave unchanged. */
      paused_until: string | null;
    }>,
  ) => patchJson<Schedule>(`/api/schedules/${id}`, body),

  deleteSchedule: (id: number) => deleteJson<void>(`/api/schedules/${id}`),

  listRuns: (entityType: "query" | "monitor", entityId: number) =>
    apiFetch<Run[]>(`/api/runs?entity_type=${entityType}&entity_id=${entityId}`),

  /** Manual refresh. 201 new / 200 already-in-flight; both return a `Run`. */
  runNow: (entityType: "query" | "monitor", entityId: number) =>
    postJson<Run>(`/api/${entityType === "query" ? "queries" : "monitors"}/${entityId}/run`),

  getQueryConsumers: (id: number) => apiFetch<QueryConsumers>(`/api/queries/${id}/consumers`),

  /** Per-run CSV attachment URL, rendered as `<a href download>` (same-origin navigation, so the
   * session cookie rides along). */
  runResultCsvUrl: (runId: number) => `${API_BASE_URL}/api/runs/${runId}/result.csv`,

  /** SSE ask loop; resolves when the stream ends. Non-2xx throws `ApiError`; aborting `signal` rejects with `AbortError`.
   * Unknown events and malformed payloads are skipped. */
  askStream: (
    body: { data_source_id: number; question: string; existing_sql?: string; error?: string },
    onEvent: (event: AskEvent) => void,
    signal?: AbortSignal,
  ): Promise<void> => streamAsk("/api/queries/ask", body, onEvent, signal),

  /** Ask stream continued through chart choice and widget creation. */
  askWidgetStream: (
    dashboardId: number,
    body: { data_source_id: number; prompt: string },
    onEvent: (event: AskEvent) => void,
    signal?: AbortSignal,
  ): Promise<void> => streamAsk(`/api/dashboards/${dashboardId}/ai_widget`, body, onEvent, signal),

  /** Ask stream continued into a drafted monitor suggestion. */
  askMonitorStream: (
    body: { data_source_id: number; prompt: string },
    onEvent: (event: AskEvent) => void,
    signal?: AbortSignal,
  ): Promise<void> => streamAsk("/api/monitors/ask", body, onEvent, signal),

  suggestVisualization: (
    body: {
      columns: { name: string; friendly_type: string }[];
      row_count: number;
    },
    signal?: AbortSignal,
  ) => postJson<SuggestVisualizationResponse>("/api/queries/suggest_visualization", body, { signal }),

  suggestMonitors: (dataSourceId: number, body: { tables: string[] }, signal?: AbortSignal) =>
    postJson<SuggestMonitorsResponse>(`/api/data_sources/${dataSourceId}/suggest_monitors`, body, {
      signal,
    }),

  /** 409s surface as `ApiError` with `errorCode` `query_unpublished` / `no_result`. */
  suggestQueryMonitors: (queryId: number, body: { prompt?: string }, signal?: AbortSignal) =>
    postJson<SuggestQueryMonitorsResponse>(`/api/queries/${queryId}/suggest_monitors`, body, {
      signal,
    }),

  getQueryVersions: (id: number) => apiFetch<QueryVersion[]>(`/api/queries/${id}/version`),

  revertQuery: (id: number, target: { change_id: number } | { version: number }) =>
    postJson<Query>(`/api/queries/${id}/revert`, target),

  forkQuery: (id: number) => postJson<Query>(`/api/queries/${id}/fork`),

  // `options` lets a caller holding parameter definitions client-side pass them inline so `{{ }}`
  // tokens still substitute; omit for plain ad-hoc SQL.
  executeAdHoc: (body: {
    data_source_id: number;
    query: string;
    parameters?: Record<string, unknown>;
    options?: QueryOptions;
    max_age?: number;
    query_id?: number;
  }) => postJson<ExecuteResponse>("/api/query_results", body),

  getQueryResult: (id: number) =>
    apiFetch<{ query_result: QueryResult }>(`/api/query_results/${id}`),

  /** Fetched, not navigated to, so a non-2xx (expired result, 403) stays a caught error inside the
   * SPA. */
  downloadQueryResult: (id: number, ext: string) =>
    apiFetch<Blob>(`/api/query_results/${id}.${ext}`, { responseType: "blob" }),

  getJob: (id: string) => apiFetch<{ job: Job }>(`/api/jobs/${id}`),

  cancelJob: (id: string) => deleteJson<void>(`/api/jobs/${id}`),

  starQuery: (id: number) => postJson<void>(`/api/queries/${id}/favorite`),
  unstarQuery: (id: number) => deleteJson<void>(`/api/queries/${id}/favorite`),
  listFavoriteQueries: () => apiFetch<Query[]>("/api/queries/favorites"),

  listVisualizations: (queryId: number) =>
    apiFetch<Visualization[]>(`/api/queries/${queryId}/visualizations`),

  createVisualization: (body: {
    query_id: number;
    type: string;
    name: string;
    description?: string;
    options: Record<string, unknown>;
  }) => postJson<Visualization>("/api/visualizations", body),

  updateVisualization: (id: number, body: Partial<{ name: string; options: Record<string, unknown> }>) =>
    postJson<Visualization>(`/api/visualizations/${id}`, body),

  deleteVisualization: (id: number) => deleteJson<void>(`/api/visualizations/${id}`),

  listSnippets: () => apiFetch<Snippet[]>("/api/query_snippets"),

  createSnippet: (body: { trigger: string; description?: string; snippet: string }) =>
    postJson<Snippet>("/api/query_snippets", body),

  updateSnippet: (
    id: number,
    body: Partial<{ trigger: string; description: string; snippet: string }>,
  ) => postJson<Snippet>(`/api/query_snippets/${id}`, body),

  deleteSnippet: (id: number) =>
    deleteJson<void>(`/api/query_snippets/${id}`),

  listDashboards: (includeArchived = false, q?: string, filter?: ScopeFilter) =>
    apiFetch<DashboardListItem[]>(
      `/api/dashboards${queryString({ include_archived: includeArchived, q, filter })}`,
    ),

  getDashboardTags: () =>
    apiFetch<{ tags: string[] }>("/api/dashboards/tags"),

  getDashboard: (slug: string, opts?: { draft?: boolean }) =>
    apiFetch<Dashboard>(`/api/dashboards/${slug}${opts?.draft ? "?draft=true" : ""}`),

  createDashboard: (name: string) => postJson<Dashboard>("/api/dashboards", { name }),

  updateDashboard: (slug: string, body: object) =>
    postJson<Dashboard>(`/api/dashboards/${slug}`, body),

  deleteDashboard: (slug: string) => deleteJson<void>(`/api/dashboards/${slug}`),

  publishDashboard: (slug: string) => postJson<Dashboard>(`/api/dashboards/${slug}/publish`),

  discardDashboardChanges: (slug: string) => postJson<Dashboard>(`/api/dashboards/${slug}/discard`),

  unpublishDashboard: (slug: string) => postJson<Dashboard>(`/api/dashboards/${slug}/unpublish`),

  saveDashboardLayout: (slug: string, positions: { widget_id: number; position: WidgetPosition }[]) =>
    putJson<Widget[]>(`/api/dashboards/${slug}/layout`, { positions }),

  starDashboard: (id: number) => postJson<void>(`/api/dashboards/${id}/favorite`),
  unstarDashboard: (id: number) => deleteJson<void>(`/api/dashboards/${id}/favorite`),

  createWidget: (body: object) => postJson<Widget>("/api/widgets", body),
  updateWidget: (id: number, body: object) => postJson<Widget>(`/api/widgets/${id}`, body),
  deleteWidget: (id: number) => deleteJson<void>(`/api/widgets/${id}`),

  listMonitors: (filter?: ScopeFilter, queryId?: number) =>
    apiFetch<Monitor[]>(`/api/monitors${queryString({ filter, query_id: queryId == null ? undefined : String(queryId) })}`),
  getMonitor: (id: number) => apiFetch<Monitor>(`/api/monitors/${id}`),
  createMonitor: (body: { query_id: number; name: string; options: MonitorOptions; rearm?: number }) =>
    postJson<Monitor>("/api/monitors", body),
  updateMonitor: (id: number, body: Partial<{ name: string; options: MonitorOptions; rearm: number | null }>) =>
    postJson<Monitor>(`/api/monitors/${id}`, body),
  getMonitorObservations: (id: number, limit = 100) =>
    apiFetch<MonitorObservation[]>(`/api/monitors/${id}/observations?limit=${limit}`),
  deleteMonitor: (id: number) => deleteJson<void>(`/api/monitors/${id}`),
  muteMonitor: (id: number) => postJson<Monitor>(`/api/monitors/${id}/mute`, {}),
  unmuteMonitor: (id: number) => postJson<Monitor>(`/api/monitors/${id}/unmute`, {}),
  snoozeMonitor: (id: number, minutes: number) =>
    postJson<Monitor>(`/api/monitors/${id}/snooze`, { minutes }),
  listMonitorSubscriptions: (id: number) => apiFetch<MonitorSubscription[]>(`/api/monitors/${id}/subscriptions`),
  createMonitorSubscription: (id: number, body: { destination_id?: number | null }) =>
    postJson<MonitorSubscription>(`/api/monitors/${id}/subscriptions`, body),
  deleteMonitorSubscription: (subId: number) => deleteJson<void>(`/api/monitor_subscriptions/${subId}`),
  notifyMonitorSubscription: (monitorId: number, subId: number) =>
    postJson<void>(`/api/monitors/${monitorId}/subscriptions/${subId}/notify`, {}),
  listMonitorDeliveries: (id: number) => apiFetch<MonitorDelivery[]>(`/api/monitors/${id}/deliveries`),
  setDigest: (body: { enabled: boolean; hour?: number; timezone?: string }) =>
    postJson<{ enabled: boolean; hour: number | null; timezone: string | null }>(
      "/api/users/me/digest",
      body,
    ),
  getMonitorEscalation: (id: number) => apiFetch<EscalationStep[]>(`/api/monitors/${id}/escalation`),
  putMonitorEscalation: (id: number, steps: EscalationStep[]) =>
    putJson<EscalationStep[]>(`/api/monitors/${id}/escalation`, steps),
  acknowledgeMonitor: (id: number) => postJson<Monitor>(`/api/monitors/${id}/ack`, {}),
  listDestinations: () => apiFetch<Destination[]>("/api/destinations"),
  listDestinationTypes: () => apiFetch<DestinationType[]>("/api/destinations/types"),
  createDestination: (body: { name: string; type: string; options: Record<string, unknown> }) =>
    postJson<Destination>("/api/destinations", body),
  getDestination: (id: number) => apiFetch<Destination>(`/api/destinations/${id}`),
  updateDestination: (id: number, body: { name?: string; options?: Record<string, unknown> }) =>
    postJson<Destination>(`/api/destinations/${id}`, body),
  deleteDestination: (id: number) => deleteJson<void>(`/api/destinations/${id}`),
  testDestination: (
    body: { type: string; options: Record<string, unknown> } | { id: number },
  ) => postJson<TestConnectionResponse>("/api/destinations/test", body),

  shareDashboard: (slug: string) => postJson<ShareInfo>(`/api/dashboards/${slug}/share`, {}),
  revokeShareDashboard: (slug: string) => deleteJson<void>(`/api/dashboards/${slug}/share`),
  // Deprecated shim: served as the stripped `PublicEmbedResponse`, not the full `Dashboard`.
  getPublicDashboard: (token: string) =>
    apiFetch<PublicEmbedResponse>(`/api/dashboards/public/${token}`),

  listAcl: (objectType: ObjectType, id: number) =>
    apiFetch<AccessPermission[]>(`/api/${OBJECT_TYPE_PATH[objectType]}/${id}/acl`),
  grantAcl: (
    objectType: ObjectType,
    id: number,
    body: { grantee_id?: number; grantee_group_id?: number; access_type: ObjectRole },
  ) => postJson<AccessPermission>(`/api/${OBJECT_TYPE_PATH[objectType]}/${id}/acl`, body),
  revokeAcl: (objectType: ObjectType, id: number, permId: number) =>
    deleteJson<void>(`/api/${OBJECT_TYPE_PATH[objectType]}/${id}/acl/${permId}`),

  setVisibility: (objectType: ObjectType, id: number, visibility: Visibility) =>
    putJson<void>(`/api/${OBJECT_TYPE_PATH[objectType]}/${id}/visibility`, { visibility }),

  getPublicEmbed: (token: string) => apiFetch<PublicEmbedResponse>(`/api/embed/${token}`),

  createEmbed: (body: CreateEmbedInput) => postJson<Embed>("/api/embeds", body),

  listEmbeds: (objectType: "dashboard" | "visualization", objectId: number) =>
    apiFetch<Embed[]>(`/api/embeds?object_type=${objectType}&object_id=${objectId}`),

  revokeEmbed: (id: number) => deleteJson<void>(`/api/embeds/${id}`),

  countActivePublicEmbeds: (queryId: number) =>
    apiFetch<{ count: number }>(`/api/embeds/active_count?query_id=${queryId}`),

  getOrgEmbedSettings: () => apiFetch<OrgEmbedSettings>("/api/orgs/me/embed_settings"),

  setOrgEmbedSettings: (body: { public_urls_enabled: boolean }) =>
    postJson<OrgEmbedSettings>("/api/orgs/me/embed_settings", body),

  listUsers: (includeDisabled = false) =>
    apiFetch<UserDetails[]>(`/api/users${includeDisabled ? "?include_disabled=true" : ""}`),

  getUser: (id: number) => apiFetch<UserDetails>(`/api/users/${id}`),

  inviteUser: (body: { name: string; email: string; groups?: number[] }) =>
    postJson<UserDetails>("/api/users", body),

  // Email is not editable here (the backend ignores it), so it's dropped from the type.
  updateUser: (id: number, body: Partial<{ name: string; groups: number[]; disabled: boolean }>) =>
    postJson<UserDetails>(`/api/users/${id}`, body),

  deleteUser: (id: number) => deleteJson<void>(`/api/users/${id}`),

  resendInvitation: (id: number) => postJson<UserDetails>(`/api/users/${id}/resend_invitation`, {}),

  revokeInvitation: (id: number) => deleteJson<void>(`/api/users/${id}/invitation`),

  regenerateApiKey: (id: number) =>
    postJson<{ api_key: string }>(`/api/users/${id}/regenerate_api_key`, {}),

  verifyInvitation: (token: string) =>
    apiFetch<{ email: string; name: string }>(`/api/users/invitation/verify?token=${encodeURIComponent(token)}`),

  acceptInvitation: (body: { token: string; name: string; password: string }) =>
    postJson<UserDetails>("/api/users/invitation/accept", body),

  // A 401 here means a wrong current_password, not an expired session — skip the global handler.
  changePassword: (body: { current_password: string; new_password: string }) =>
    postJson<void>("/api/users/me/password", body, { skipSessionExpiry: true }),

  forgotPassword: (email: string) =>
    postJson<void>("/api/forgot_password", { email }),

  resetPassword: (body: { token: string; password: string }) =>
    postJson<void>("/api/reset_password", body),

  listGroups: () => apiFetch<Group[]>("/api/groups"),
  getGroup: (id: number) => apiFetch<Group>(`/api/groups/${id}`),
  createGroup: (body: { name: string }) => postJson<Group>("/api/groups", body),
  updateGroup: (id: number, body: Partial<{ name: string }>) =>
    postJson<Group>(`/api/groups/${id}`, body),
  deleteGroup: (id: number) => deleteJson<void>(`/api/groups/${id}`),
  listGroupMembers: (id: number) => apiFetch<GroupMember[]>(`/api/groups/${id}/members`),
  addGroupMember: (id: number, userId: number) =>
    postJson<void>(`/api/groups/${id}/members`, { user_id: userId }),
  removeGroupMember: (id: number, userId: number) =>
    deleteJson<void>(`/api/groups/${id}/members/${userId}`),
  listGroupDataSources: (id: number) =>
    apiFetch<GroupDataSourceAccess>(`/api/groups/${id}/data_sources`),
  assignGroupDataSource: (id: number, body: { data_source_id: number }) =>
    postJson<void>(`/api/groups/${id}/data_sources`, body),
  removeGroupDataSource: (id: number, dsId: number) =>
    deleteJson<void>(`/api/groups/${id}/data_sources/${dsId}`),
  setGroupDataSourceRestricted: (id: number, restricted: boolean) =>
    putJson<{ ok: boolean; restricted: boolean }>(`/api/groups/${id}/data_source_access`, {
      restricted,
    }),

  listPermissions: () => apiFetch<Permission[]>("/api/permissions"),
  listRoles: () => apiFetch<Role[]>("/api/roles"),
  getRole: (id: number) => apiFetch<Role>(`/api/roles/${id}`),
  createRole: (body: { name: string; permissions: string[] }) => postJson<Role>("/api/roles", body),
  updateRole: (id: number, body: Partial<{ name: string; permissions: string[] }>) =>
    postJson<Role>(`/api/roles/${id}`, body),
  deleteRole: (id: number) => deleteJson<void>(`/api/roles/${id}`),
  listGroupRoles: (id: number) => apiFetch<GroupRole[]>(`/api/groups/${id}/roles`),
  addGroupRole: (id: number, roleId: number) => postJson<void>(`/api/groups/${id}/roles`, { role_id: roleId }),
  removeGroupRole: (id: number, roleId: number) => deleteJson<void>(`/api/groups/${id}/roles/${roleId}`),
  getUserEffectivePermissions: (id: number) =>
    apiFetch<EffectivePermissions>(`/api/users/${id}/effective_permissions`),

  getOrganization: () => apiFetch<OrgSettings>("/api/organization"),
  updateOrganization: (body: Partial<{ name: string; settings: Record<string, unknown> }>) =>
    postJson<OrgSettings>("/api/organization", body),

  getAiSettings: () => apiFetch<AiSettings>("/api/organization/ai"),
  updateAiSettings: (body: {
    /** Single-provider API key field; prefer `providers`. */
    gemini_api_key?: string | null;
    rate_limit_per_min?: number | null;
    providers?: Record<string, { api_key?: string | null; model?: string | null; base_url?: string | null }>;
    provider_order?: string[];
    fallback_enabled?: boolean;
    include_sample_values?: boolean | null;
    explain_alerts?: boolean;
    dashboard_narratives?: boolean;
    /** `null` clears the cap; omit to leave it unchanged. */
    monthly_budget_usd?: number | null;
  }) => postJson<AiSettings>("/api/organization/ai", body),
  testAiKey: (body?: { provider?: string; api_key?: string }) =>
    postJson<TestKeyResult>("/api/organization/ai/test", body ?? {}),
  explainDashboard: (id: number) => postJson<ExplainDashboardResponse>(`/api/dashboards/${id}/narrative`, {}),
  getDashboardNarrative: (id: number) =>
    apiFetch<{ narrative: DashboardNarrative }>(`/api/dashboards/${id}/narrative`),
  getAiUsage: (days?: number) =>
    apiFetch<AiUsageResponse>(`/api/organization/ai/usage${days ? `?days=${days}` : ""}`),

  listEvents: (params?: {
    page?: number;
    page_size?: number;
    user_id?: number;
    action?: string;
    from?: string;
    to?: string;
  }) => {
    const { page, page_size, user_id, action, from, to } = params ?? {};
    return apiFetchWithTotal<AuditEvent[]>(
      `/api/events${queryString({ page, page_size, user_id, action, from, to })}`,
    ).then(({ data, total }) => ({ events: data, total }));
  },

  getStatus: () => apiFetch<SystemStatus>("/api/status"),
};
