import type React from "react";
import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { flushSync } from "react-dom";
import { useMutation, useQuery, useQueryClient, type QueryClient } from "@tanstack/react-query";
import { Link, useBlocker, useLocation, useNavigate, useParams } from "react-router";
import type { BlockerFunction } from "react-router";
import CodeMirror from "@uiw/react-codemirror";
import { sql, PostgreSQL } from "@codemirror/lang-sql";
import { EditorView } from "@codemirror/view";
import {
  Archive,
  ArchiveRestore,
  BellRing,
  CalendarClock,
  CalendarPlus,
  CircleStop,
  Database,
  GitFork,
  History,
  Info,
  Share2,
  Star,
  Sparkles,
  User,
} from "lucide-react";
import { Avatar, AvatarFallback } from "@/components/ui/avatar";
import { Badge } from "@/components/ui/badge";
import { ResultFootnote, hasResultFootnote } from "@/components/ResultFootnote";
import { Button } from "@/components/ui/button";
import { Dialog, DialogContent, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog";
import { ConfirmDialog } from "@/components/ui/confirm-dialog";
import { Sheet, SheetContent, SheetTitle } from "@/components/ui/sheet";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Loader, PageLoader } from "@/components/ui/loader";
import { Textarea } from "@/components/ui/textarea";
import { ActionBar, type ActionItem } from "@/components/common/ActionBar";
import { StateBadge, UnpublishedBadge } from "@/components/common/StateBadge";
import { EmptyState } from "@/components/common/EmptyState";
import { TagEditor } from "@/components/common/TagEditor";
import { DocsLink } from "@/components/common/DocsLink";
import { DataSourcePicker } from "@/components/query-editor/DataSourcePicker";
import { SqlEditor, type SqlEditorHandle } from "@/components/query-editor/SqlEditor";
import { SchemaBrowser } from "@/components/data-sources/SchemaBrowser";
import { ResultsTable } from "@/components/query-editor/ResultsTable";
import { ExecuteStatusBanner } from "@/components/query-editor/ExecuteStatusBanner";
import { ParametersBar, useDefaultParameterValues } from "@/components/query-editor/ParametersBar";
import { SnippetMenu } from "@/components/query-editor/SnippetMenu";
import { VersionHistoryPanel } from "@/components/query-editor/VersionHistoryPanel";
import { ExportMenu } from "@/components/query-editor/ExportMenu";
import { executionStatusIcon } from "@/components/query-editor/ExecutionStatusIndicator";
import { VizPanel } from "@/components/visualizations/VizPanel";
import { FreshnessChip } from "@/components/scheduling/FreshnessChip";
import { ScheduleSection, QUERY_FLAVOR } from "@/components/scheduling/ScheduleSection";
import { ShareDialog } from "@/components/sharing/ShareDialog";
import { initials } from "@/components/sharing/OwnerChip";
import { BackButton } from "@/components/nav/BackButton";
import { SuggestMonitorsPane, NO_RESULT_REASON, UNPUBLISHED_REASON, UNPUBLISHED_REASON_VIEWER } from "@/components/monitors/SuggestMonitorsPane";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { useDocumentTitle } from "@/lib/useDocumentTitle";
import { ApiError, ConflictError, api, apiErrorMessage } from "@/lib/api";
import { aiErrorMessage } from "@/lib/ai";
import { lastProblem, useAskRun } from "@/components/ai/askRun";
import { AiSettingsLink, AskDisclosure, AskPreviewGrid, AskTimeline, ExhaustedWarning } from "@/components/ai/AskTimeline";
import { useAuth } from "@/lib/auth";
import { usePermissions } from "@/lib/permissions";
import { usePollJob } from "@/lib/jobs";
import { cn } from "@/lib/utils";
import { effectiveState, publishDialogCopy } from "@/lib/effectiveState";
import { useAutosave } from "@/hooks/useAutosave";
import { useTransientMessage } from "@/hooks/useTransientMessage";
import { exactTs, isSchedulePaused, timeAgo, useNow } from "@/lib/scheduling";
import type {
  Monitor,
  Query,
  QueryOptions,
  QueryParameterDefinition,
  QueryResult,
  Schedule,
  SuggestVisualizationResponse,
} from "@/types/api";

interface EditorBuffer {
  name: string;
  description: string;
  query: string;
  data_source_id: number;
  options: QueryOptions;
}

const SUGGESTED_PROMPTS = [
  "Count rows in each table",
  "Show the 10 most recent records",
  "Top values by count",
];

const EDITOR_TABS = [
  ["sql", "SQL Editor"],
  ["ai", "✨ Ask with AI"],
] as const;

function isScaffoldSql(value: string): boolean {
  const trimmed = value.trim();
  return trimmed === "" || trimmed === "SELECT 1";
}

// Module-level: a new array reference makes `@uiw/react-codemirror` reconfigure every extension.
const GENERATED_SQL_EXTENSIONS = [
  sql({ dialect: PostgreSQL }),
  EditorView.lineWrapping,
  EditorView.theme({ ".cm-content": { padding: "0.75rem", fontSize: "0.75rem" } }),
];
const GENERATED_SQL_BASIC_SETUP = { lineNumbers: false, foldGutter: false, highlightActiveLine: false };

/** `--ask-avail` is the viewport height below the Ask panel top; 188px is the fixed chrome, the rest
 * is split between SQL preview and grid. Unset (narrow widths) falls back to fixed caps. */
const ASK_SQL_MAX_HEIGHT = "clamp(96px, calc((var(--ask-avail, 9999px) - 188px) / 2), 200px)";

function GeneratedSqlView({ sql: value }: { sql: string }) {
  return (
    <div data-testid="ask-sql-preview">
      <CodeMirror
        value={value}
        editable={false}
        basicSetup={GENERATED_SQL_BASIC_SETUP}
        extensions={GENERATED_SQL_EXTENSIONS}
        maxHeight={ASK_SQL_MAX_HEIGHT}
        className="overflow-hidden rounded-md border border-border"
      />
    </div>
  );
}

/** Muted overrides the schedule's own on/off — a monitor can be enabled yet muted. */
function monitorScheduleState(monitor: Monitor, schedule: Schedule | null): string {
  if (monitor.options.muted) return "Muted";
  if (!schedule) return "—";
  if (isSchedulePaused(schedule)) return "Paused";
  return schedule.enabled ? "Active" : "Paused";
}

const EXECUTE_BLOCKED_COPY: Record<"capability" | "data_source" | "both", string> = {
  data_source:
    "You can open this query, but you don't have access to its data source, so it can't run. Ask an admin for access to the data source.",
  capability:
    "Your current role doesn't allow running queries. Ask an admin to check your roles and permissions.",
  both:
    "You have limited access to this query and its data source, so it can't run. Ask an admin to check your roles and permissions.",
};

/** The only persistence signal (no Save button); Retry only once the server is unreachable. */
function AutosaveIndicator({
  status,
  onRetry,
  idleHint,
}: {
  status: "idle" | "saving" | "saved" | "failed" | "offline" | "conflict";
  onRetry: () => void;
  idleHint?: string;
}) {
  if (status === "idle") {
    return idleHint ? (
      <span className="shrink-0 text-xs text-muted-foreground">{idleHint}</span>
    ) : null;
  }
  if (status === "saving") {
    return (
      <span role="status" className="flex shrink-0 items-center gap-1.5 text-xs text-muted-foreground">
        <Loader size={14} />
        Saving…
      </span>
    );
  }
  if (status === "conflict") {
    return (
      <span role="status" className="flex shrink-0 items-center gap-1.5 text-xs text-destructive">
        Conflict — reload to continue
      </span>
    );
  }
  if (status === "failed" || status === "offline") {
    return (
      <span
        role="status"
        className={cn(
          "flex shrink-0 items-center gap-1.5 text-xs",
          status === "failed" ? "text-destructive" : "text-warning",
        )}
      >
        {status === "failed" ? "Save failed" : "Offline — not saved"}
        <button type="button" className="cursor-pointer underline" onClick={onRetry}>
          Retry
        </button>
      </span>
    );
  }
  return <span role="status" className="shrink-0 text-xs text-muted-foreground">Saved</span>;
}

/** A run just succeeded for a saved query: refresh its status/row_count and the list. Invalidation is
 * async, so also patch the fields Publish reads, so a dialog opened in the gap sees current data. */
function markRunSucceeded(queryClient: QueryClient, queryId: number) {
  queryClient.setQueryData<Query>(["query", queryId], (old) =>
    old ? { ...old, last_execution_status: "Success", last_execution_at: new Date().toISOString() } : old,
  );
  queryClient.invalidateQueries({ queryKey: ["query", queryId] });
  queryClient.invalidateQueries({ queryKey: ["queries"] });
}

type MetaTooltipRow = {
  key: string;
  icon: React.ReactNode;
  label?: string;
  value: React.ReactNode;
};

function MetaTooltipCard({ title, rows }: { title: string; rows: MetaTooltipRow[] }) {
  return (
    <>
      <div className="px-3 pt-2 pb-1.5 text-[10px] font-bold tracking-wide text-muted-foreground uppercase">
        {title}
      </div>
      {rows.map((row, i) => (
        <div
          key={row.key}
          className={cn("flex items-center gap-2 px-3 py-1.5 text-xs", i > 0 && "border-t border-border")}
        >
          {row.icon}
          {row.label && <span className="text-muted-foreground">{row.label}</span>}
          <span className="font-medium text-foreground">{row.value}</span>
        </div>
      ))}
    </>
  );
}

export function QueryEditorPage() {
  const params = useParams<{ id: string }>();
  const isNew = params.id === undefined || params.id === "new";
  const queryId = isNew ? null : Number(params.id);
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const editorRef = useRef<SqlEditorHandle>(null);
  // Set before a self-created query swaps the URL to its new id, so the hydrate effect does not
  // overwrite the live buffer with the fetched copy.
  const selfCreatedIdRef = useRef<number | null>(null);
  // Read at call time: a save chained off the create runs before re-render, while `queryId` is
  // still null, and would POST a duplicate row.
  const createdIdRef = useRef<number | null>(null);
  // New and :id share one route (no remount), so reset the id for the next draft.
  useEffect(() => {
    if (queryId == null) createdIdRef.current = null;
  }, [queryId]);
  // The unmount-flush save can outlive the component; its create path must not navigate then.
  const mountedRef = useRef(true);
  useEffect(() => {
    // StrictMode double-invokes mount effects; this re-sets the ref after the dev cleanup.
    mountedRef.current = true;
    return () => {
      mountedRef.current = false;
    };
  }, []);

  // Non-empty default so a saved query never shows a blank name; `nameTouched` is what gates
  // autosave.
  const [name, setName] = useState("Untitled query");
  // True once the user edits the name, so autosave cannot create a query from SQL alone.
  const [nameTouched, setNameTouched] = useState(false);
  const [description, setDescription] = useState("");
  const [sql, setSql] = useState("SELECT 1");
  const location = useLocation();
  // Prefill handed over by "Add with AI" on a dashboard; the Ask tab opens but never auto-runs.
  const [prefill] = useState(() => {
    const s = location.state as { askQuestion?: unknown; dataSourceId?: unknown } | null;
    return typeof s?.askQuestion === "string"
      ? { question: s.askQuestion, dataSourceId: typeof s.dataSourceId === "number" ? s.dataSourceId : null }
      : null;
  });
  const [dataSourceId, setDataSourceId] = useState<number | null>(prefill?.dataSourceId ?? null);
  const [saveError, setSaveError] = useState<string | null>(null);
  // Another editor advanced the row (409): the fix is reload, not retry; keep the local buffer
  // until then.
  const [conflict, setConflict] = useState(false);
  // Confirms a revert/restore landed; the triggering dialog has already closed.
  const [historyFeedback, setHistoryFeedback] = useTransientMessage<string>();
  const [execError, setExecError] = useState<string | null>(null);
  const [dsAccessDenied, setDsAccessDenied] = useState(false);
  const [activeJobId, setActiveJobId] = useState<string | null>(null);
  const [result, setResult] = useState<QueryResult | null>(null);
  const [resultForSql, setResultForSql] = useState<string | null>(null);
  const [parameterDefinitions, setParameterDefinitions] = useState<QueryParameterDefinition[]>(
    [],
  );
  const [parameterValues, setParameterValues] = useState<Record<string, unknown>>({});
  const [historyOpen, setHistoryOpen] = useState(false);
  const [schemaOpen, setSchemaOpen] = useState(false);
  const [shareOpen, setShareOpen] = useState(false);
  const [scheduleExpanded, setScheduleExpanded] = useState(false);
  const [scheduleFocusRunId, setScheduleFocusRunId] = useState<number | null>(null);

  const [editorTab, setEditorTab] = useState<"sql" | "ai">(prefill ? "ai" : "sql");
  const [aiQuestion, setAiQuestion] = useState(prefill?.question ?? "");
  const ask = useAskRun();
  const askState = ask.state;
  const fixContextRef = useRef<{ sql: string; error: string; question: string } | null>(null);
  const askRunning = askState.phase === "running";
  const askPanelRef = useRef<HTMLDivElement>(null);
  const askStarted = askState.phase !== "idle";
  useLayoutEffect(() => {
    const el = askPanelRef.current;
    if (!el) return;
    const measure = () => {
      if (window.matchMedia("(min-width: 640px)").matches) {
        const top = el.getBoundingClientRect().top + window.scrollY;
        el.style.setProperty("--ask-avail", `${window.innerHeight - top - 16}px`);
      } else {
        el.style.removeProperty("--ask-avail");
      }
    };
    measure();
    window.addEventListener("resize", measure);
    return () => window.removeEventListener("resize", measure);
  }, [editorTab, askState.phase]);
  const aiGenerated =
    askState.phase === "done" && askState.final
      ? { sql: askState.final.sql, explanation: askState.final.explanation ?? undefined }
      : null;
  const askProblem = lastProblem(askState);
  const [applyConfirmOpen, setApplyConfirmOpen] = useState(false);

  const [autoVizError, setAutoVizError] = useState<string | null>(null);
  const [pendingAutoCreate, setPendingAutoCreate] = useState<{
    type: string;
    name: string;
    options: Record<string, unknown>;
  } | null>(null);
  const vizAbortRef = useRef<AbortController | null>(null);

  const { clientConfig } = useAuth();
  const aiEnabled = clientConfig?.ai_sql_generation ?? false;
  const { has } = usePermissions();
  const canCreate = has("create_query");
  const canCreateMonitor = has("create_monitor");
  const showSuggestMonitors = aiEnabled && (clientConfig?.ai_monitor_suggestions ?? false) && canCreateMonitor;
  const [suggestMonitorsOpen, setSuggestMonitorsOpen] = useState(false);

  const dataSourcesQuery = useQuery({
    queryKey: ["data-sources"],
    queryFn: () => api.listDataSources(),
  });

  const schemaQuery = useQuery({
    queryKey: ["data-source-schema", dataSourceId],
    queryFn: () => api.getDataSourceSchema(dataSourceId!, false),
    enabled: dataSourceId != null,
  });

  const existingQueryQuery = useQuery({
    queryKey: ["query", queryId],
    queryFn: () => api.getQuery(queryId!),
    enabled: queryId != null,
  });

  useDocumentTitle(
    isNew
      ? "New query · arcodash"
      : existingQueryQuery.data
        ? `${existingQueryQuery.data.name || "Untitled query"} · arcodash`
        : "Query · arcodash",
  );

  // Stored result of a saved query, loaded once; gated on a result pointer (a never-run query would
  // 404).
  const latestResultQuery = useQuery({
    queryKey: ["query-latest-result", queryId],
    queryFn: () => api.getLatestQueryResult(queryId!),
    enabled: queryId != null && existingQueryQuery.data?.latest_query_data_id != null,
    retry: false,
  });

  const tagsQuery = useQuery({
    queryKey: ["query-tags"],
    queryFn: () => api.getQueryTags(),
  });

  const monitorsForQueryQuery = useQuery({
    queryKey: ["monitors", "query", queryId],
    queryFn: () => api.listMonitors(undefined, queryId!),
    enabled: queryId != null,
  });

  // Shares the ["snippets"] cache key with SnippetMenu; gated on view_snippets.
  const snippetsQuery = useQuery({
    queryKey: ["snippets"],
    queryFn: () => api.listSnippets(),
    enabled: has("view_snippets"),
  });

  const monitorSchedulesQuery = useQuery({
    queryKey: ["schedules", "all"],
    queryFn: () => api.listSchedules(),
    enabled: queryId != null,
  });
  const scheduleForMonitor = (monitorId: number) =>
    monitorSchedulesQuery.data?.find((s) => s.entity_type === "monitor" && s.entity_id === monitorId) ??
    null;

  // Shares VizPanel's ["visualizations", queryId] key so a never-run fork can still show it owns
  // visualizations.
  const savedVisualizationsQuery = useQuery({
    queryKey: ["visualizations", queryId],
    queryFn: () => api.listVisualizations(queryId!),
    enabled: queryId != null,
  });
  const savedVisualizations = savedVisualizationsQuery.data ?? [];

  // `caller.can_edit` is the backend's capability∩reach edit decision; a new unsaved query is
  // capability-only.
  const canSave = isNew ? has("create_query") : (existingQueryQuery.data?.caller.can_edit ?? false);
  const canEditExisting = existingQueryQuery.data?.caller.can_edit ?? false;
  // Share is owner-tier on top of edit-tier (like `can_delete`).
  const canManageSharing =
    canEditExisting && existingQueryQuery.data?.caller.object_role === "owner";
  // Execute follows the server's per-object decision for saved queries; new queries are
  // capability-only.
  const canExecute = isNew ? has("execute_query") : (existingQueryQuery.data?.caller.can_execute ?? false);
  const executeBlockedReason = isNew ? undefined : existingQueryQuery.data?.caller.execute_blocked_reason;

  /** Replaces the editor buffer with `q` and clears the previous run's output. */
  function hydrateBuffer(q: Query) {
    setName(q.name);
    setDescription(q.description ?? "");
    setSql(q.query);
    setDataSourceId(q.data_source_id);
    setParameterDefinitions(q.options?.parameters ?? []);
    setResult(null);
    setResultForSql(null);
    setExecError(null);
  }

  // Hydrate when switching to a different query. Keyed on id only so an autosave's `setQueryData`
  // cannot stomp in-flight typing; same-id changes (version revert) go through `handleReload`.
  // Skips the id a self-create just produced, since the live buffer is newer.
  useEffect(() => {
    if (existingQueryQuery.data?.id === selfCreatedIdRef.current) {
      selfCreatedIdRef.current = null;
      return;
    }
    if (existingQueryQuery.data) hydrateBuffer(existingQueryQuery.data);
    // eslint-disable-next-line react-hooks/exhaustive-deps -- keyed on id only, see above
  }, [existingQueryQuery.data?.id]);

  // Show the stored result once if nothing else is on screen; `resultForSql` is the stored result's
  // own SQL so staleness compares correctly. Also keyed on the hydrate id: the result fetch can win
  // the race and the hydrate effect then clears `result`, so re-check after.
  useEffect(() => {
    if (!latestResultQuery.data || activeJobId !== null) return;
    const stored = latestResultQuery.data.query_result;
    setResult((prev) => (prev === null ? stored : prev));
    setResultForSql((prev) => (prev === null ? stored.query : prev));
  }, [latestResultQuery.data, existingQueryQuery.data?.id, activeJobId]);

  useDefaultParameterValues(parameterDefinitions, parameterValues, setParameterValues);

  useEffect(() => {
    const sources = dataSourcesQuery.data?.data_sources;
    if (isNew && dataSourceId == null && sources && sources.length > 0) {
      setDataSourceId(sources[0].id);
    }
  }, [isNew, dataSourceId, dataSourcesQuery.data]);

  const job = usePollJob(activeJobId);
  const now = useNow(false);

  // Real SQL errors arrive as a failed job, not `execError` (initial POST failures only); poll
  // errors are excluded since there is no SQL to fix.
  const jobFailureError =
    activeJobId != null && job.isFailure && !job.isPollError ? (job.error ?? "Query execution failed.") : null;
  const fixTriggerError = execError ?? jobFailureError;

  // SQL at job launch (set in onMutate) so editing during a slow run cannot skew the staleness
  // check.
  const executedSqlRef = useRef(sql);

  // Synced each render so the job-fetch effect can tell a newer Execute superseded its job.
  const activeJobIdRef = useRef(activeJobId);
  activeJobIdRef.current = activeJobId;

  // A newer Execute may start before this job's result resolves; its late response would null
  // `activeJobId` and kill the new job's polling.
  useEffect(() => {
    if (job.isSuccess && job.queryResultId != null) {
      const jobId = activeJobId;
      // Resolved now: after the fetch the URL swap has cleared the ref while `queryId` is still
      // null.
      const boundId = queryId ?? selfCreatedIdRef.current;
      api
        .getQueryResult(job.queryResultId)
        .then((res) => {
          if (activeJobIdRef.current !== jobId) return;
          setResult(res.query_result);
          setResultForSql(executedSqlRef.current);
          setActiveJobId(null);
          if (boundId != null) markRunSucceeded(queryClient, boundId);
        })
        .catch((err) => {
          if (activeJobIdRef.current !== jobId) return;
          setExecError(apiErrorMessage(err, "Failed to fetch query result"));
          setActiveJobId(null);
        });
    }
  }, [job.isSuccess, job.queryResultId, activeJobId, queryId, queryClient]);

  // The only persistence path (no Save button): creates a new query then swaps the URL, or updates
  // with expected_version (CAS against a concurrent editor).
  const persistBuffer = useCallback(
    async (buffer: EditorBuffer) => {
      // Via the ref: a save chained off this create runs before the URL swap re-renders, so
      // `queryId` is still null and would create a duplicate row.
      const targetId = queryId ?? createdIdRef.current;
      if (targetId == null) {
        const created = await api.createQuery(buffer);
        createdIdRef.current = created.id;
        selfCreatedIdRef.current = created.id;
        queryClient.setQueryData(["query", created.id], created);
        await queryClient.invalidateQueries({ queryKey: ["queries"] });
        if (mountedRef.current) navigate(`/queries/${created.id}`, { replace: true });
        return;
      }
      // Read fresh: a chained save can fire before React re-renders with the version the prior save
      // wrote.
      const cached = queryClient.getQueryData<Query>(["query", targetId]);
      // The cache entry may be evicted while a buffer is pending; refetch rather than drop the
      // write.
      const expectedVersion =
        cached?.working_version ??
        (await api.getQuery(targetId).then((fresh) => {
          queryClient.setQueryData(["query", targetId], fresh);
          return fresh.working_version;
        }));
      const updated = await api.updateQuery(targetId, { ...buffer, expected_version: expectedVersion });
      queryClient.setQueryData(["query", targetId], updated);
      await queryClient.invalidateQueries({ queryKey: ["queries"] });
    },
    [queryId, queryClient, navigate],
  );

  // A new query needs a user-given name AND real SQL before its row is created. Execute bypasses
  // this via `autosave.forceSave()`.
  const canPersist =
    canSave &&
    !!name.trim() &&
    dataSourceId != null &&
    (!isNew || (nameTouched && !isScaffoldSql(sql)));
  // Memoized: a fresh object each render defeats the hook's JSON.stringify memo and re-serializes
  // the SQL per keystroke.
  const autosavePayload = useMemo<EditorBuffer>(
    () => ({
      name,
      description,
      query: sql,
      data_source_id: dataSourceId ?? 0,
      options: { ...existingQueryQuery.data?.options, parameters: parameterDefinitions },
    }),
    [name, description, sql, dataSourceId, existingQueryQuery.data?.options, parameterDefinitions],
  );
  const autosave = useAutosave<EditorBuffer>({
    enabled: canPersist,
    payload: autosavePayload,
    save: persistBuffer,
    onConflict: () => setConflict(true),
    // The edit that first satisfies `canPersist` is worth saving, not a baseline to adopt; debounce
    // it so in-flight keystrokes land.
    baselineOnEnable: !isNew,
  });

  const forkMutation = useMutation({
    mutationFn: async () => {
      if (queryId == null) throw new Error("Save the query before forking it");
      return api.forkQuery(queryId);
    },
    onSuccess: async (forked) => {
      setSaveError(null);
      await queryClient.invalidateQueries({ queryKey: ["queries"] });
      navigate(`/queries/${forked.id}`);
    },
    onError: (err) => {
      setSaveError(apiErrorMessage(err, "Failed to fork query"));
    },
  });

  const [publishConfirmOpen, setPublishConfirmOpen] = useState(false);
  const suggestQuery = existingQueryQuery.data;
  const suggestDisabledReason = !suggestQuery
    ? null
    : !suggestQuery.published
      ? canEditExisting
        ? UNPUBLISHED_REASON
        : UNPUBLISHED_REASON_VIEWER
      : suggestQuery.latest_query_data_id == null
        ? NO_RESULT_REASON
        : null;
  const [discardConfirmOpen, setDiscardConfirmOpen] = useState(false);
  const [unpublishConfirmOpen, setUnpublishConfirmOpen] = useState(false);

  /** Publish / discard / unpublish: CAS on the working version; a 409 means reload, not retry. */
  const versionedAction = (
    call: (id: number, expectedVersion: number) => Promise<Query>,
    closeDialog: () => void,
    failure: string,
    afterSuccess?: (updated: Query) => void,
  ) => ({
    mutationFn: async () => {
      if (queryId == null || existingQueryQuery.data == null) throw new Error("Save the query first");
      return call(queryId, existingQueryQuery.data.working_version);
    },
    onSuccess: async (updated: Query) => {
      setSaveError(null);
      closeDialog();
      queryClient.setQueryData(["query", queryId], updated);
      afterSuccess?.(updated);
      await queryClient.invalidateQueries({ queryKey: ["queries"] });
    },
    onError: (err: unknown) => {
      if (err instanceof ConflictError) {
        closeDialog();
        setConflict(true);
        return;
      }
      setSaveError(apiErrorMessage(err, failure));
    },
  });

  const publishMutation = useMutation(
    versionedAction((id, v) => api.publishQuery(id, v), () => setPublishConfirmOpen(false), "Failed to publish"),
  );
  const discardMutation = useMutation(
    versionedAction(
      (id, v) => api.discardQueryChanges(id, v),
      () => setDiscardConfirmOpen(false),
      "Failed to discard changes",
      (updated) => {
        // The id-keyed hydrate will not refire; re-hydrate from the response. `markClean()` resets
        // the baseline and drops pending/in-flight saves so Retry cannot resurrect the discarded
        // draft.
        hydrateBuffer(updated);
        autosave.markClean();
      },
    ),
  );
  const unpublishMutation = useMutation(
    versionedAction((id, v) => api.unpublishQuery(id, v), () => setUnpublishConfirmOpen(false), "Failed to unpublish"),
  );

  const unpublishConsumersQuery = useQuery({
    queryKey: ["consumers", queryId],
    queryFn: () => api.getQueryConsumers(queryId!),
    enabled: unpublishConfirmOpen && queryId != null,
  });

  const toggleArchivedMutation = useMutation({
    mutationFn: async () => {
      if (queryId == null) throw new Error("Save the query first");
      const isCurrentlyArchived = existingQueryQuery.data?.is_archived ?? false;
      return api.updateQuery(queryId, { is_archived: !isCurrentlyArchived });
    },
    onSuccess: async () => {
      setSaveError(null);
      await Promise.all([
        queryClient.invalidateQueries({ queryKey: ["query", queryId] }),
        queryClient.invalidateQueries({ queryKey: ["queries"] }),
      ]);
    },
    onError: (err) => {
      setSaveError(apiErrorMessage(err, "Failed to update archive state"));
    },
  });

  const starMutation = useMutation({
    mutationFn: async () => {
      if (queryId == null) return;
      await (existingQueryQuery.data?.is_favorite ? api.unstarQuery(queryId) : api.starQuery(queryId));
    },
    onSuccess: async () => {
      await queryClient.invalidateQueries({ queryKey: ["query", queryId] });
    },
    onError: (err) => setSaveError(apiErrorMessage(err, "Failed to update favorite")),
  });

  const tagsMutation = useMutation({
    mutationFn: (tags: string[]) => api.updateQuery(queryId!, { tags }),
    onSuccess: (updated) => {
      setSaveError(null);
      queryClient.setQueryData(["query", queryId], updated);
      queryClient.invalidateQueries({ queryKey: ["queries"] });
      queryClient.invalidateQueries({ queryKey: ["query-tags"] });
    },
    onError: (err) => setSaveError(apiErrorMessage(err, "Failed to update tags")),
  });

  /** Persist parameter definitions immediately (else lost if the user runs straight away), through
   * autosave so it cannot conflict with its own save. `flushSync` commits them before `forceSave`
   * reads its payload ref. */
  async function persistParameterDefinitions(definitions: QueryParameterDefinition[]) {
    flushSync(() => setParameterDefinitions(definitions));
    // `forceSave` skips `canPersist`; skip a too-new query here, the definitions ride its first
    // real save.
    if (!canPersist) return;
    const outcome = await autosave.forceSave();
    if (outcome !== "saved") {
      throw new Error("Failed to save parameter");
    }
  }

  const executeMutation = useMutation({
    mutationFn: async () => {
      if (dataSourceId == null) throw new Error("Select a data source first");
      // Flush first: `queryId` stays null until the URL swap lands, so the run would not bind to a
      // row.
      let boundQueryId = queryId ?? selfCreatedIdRef.current;
      if (isNew && canSave && !!name.trim()) {
        await autosave.forceSave();
        boundQueryId = queryId ?? selfCreatedIdRef.current;
      }
      // Run the live editor `sql`, not the saved copy; `max_age: 0` because Execute means "run
      // now".
      return api.executeAdHoc({
        data_source_id: dataSourceId,
        query: sql,
        parameters: parameterValues,
        options: { parameters: parameterDefinitions },
        query_id: boundQueryId ?? undefined,
        max_age: 0,
      });
    },
    onMutate: () => {
      setExecError(null);
      setDsAccessDenied(false);
      setResult(null);
      setActiveJobId(null);
      // Avoids a stale `isSuccess` from a prior cancel showing this run's Stop as "Stopping…".
      cancelMutation.reset();
      executedSqlRef.current = sql;
    },
    onSuccess: (res) => {
      if ("query_result" in res && res.query_result) {
        setResult(res.query_result);
        setResultForSql(sql);
        // A cache hit skips the async job path, so mark the run here too.
        const boundId = queryId ?? selfCreatedIdRef.current;
        if (boundId != null) markRunSucceeded(queryClient, boundId);
      } else if ("job" in res && res.job) {
        setActiveJobId(res.job.id);
      }
    },
    onError: (err) => {
      if (err instanceof ApiError && err.errorCode === "data_source_access_required") {
        setDsAccessDenied(true);
        return;
      }
      setExecError(apiErrorMessage(err, "Failed to execute query"));
    },
  });

  // Nothing else on success: `usePollJob` sees CANCELLED and the banner takes over; nulling
  // `activeJobId` would kill that polling.
  const cancelMutation = useMutation({
    mutationFn: () => {
      if (activeJobId == null) throw new Error("No job to stop");
      return api.cancelJob(activeJobId);
    },
    onError: (err) => {
      // 404: the job already reached a terminal state; the poll settles on its own.
      if (err instanceof ApiError && err.status === 404) return;
      setExecError(apiErrorMessage(err, "Failed to stop the query"));
    },
  });

  // Stop unmounts when the job ends and drops focus to <body>; restore it to Execute, but only
  // then.
  const stopVisible = activeJobId != null && !job.isDone;
  const stopWasVisibleRef = useRef(false);
  useEffect(() => {
    if (stopWasVisibleRef.current && !stopVisible && document.activeElement === document.body) {
      document.querySelector<HTMLButtonElement>('[data-action="execute"]')?.focus();
    }
    stopWasVisibleRef.current = stopVisible;
  }, [stopVisible]);

  function handleInsert(text: string) {
    editorRef.current?.insertAtCursor(text);
  }

  const { reset: resetAsk } = ask;
  useEffect(() => {
    resetAsk();
  }, [dataSourceId, resetAsk]);

  function handleGenerateSql() {
    if (dataSourceId == null) return;
    const fix =
      askState.source === "fix" && fixContextRef.current?.question === aiQuestion ? fixContextRef.current : null;
    const source = fix ? "fix" : "ask";
    ask.start(
      { data_source_id: dataSourceId, question: aiQuestion, existing_sql: fix?.sql ?? sql, error: fix?.error },
      source,
    );
  }

  function applyGeneratedSql(generatedSql: string) {
    setSql(generatedSql);
    setEditorTab("sql");
    ask.reset();
    setApplyConfirmOpen(false);
  }

  function handleFixQuery() {
    if (dataSourceId == null || fixTriggerError == null) return;
    const firstLine = fixTriggerError.split("\n").find((l) => l.trim() !== "")?.trim() ?? "";
    const question = `Fix: ${firstLine}`.slice(0, 200);
    setAiQuestion(question);
    setEditorTab("ai");
    fixContextRef.current = { sql, error: fixTriggerError, question };
    ask.start({ data_source_id: dataSourceId, question, existing_sql: sql, error: fixTriggerError }, "fix");
  }

  const suggestVizMutation = useMutation({
    mutationFn: () => {
      if (!result) throw new Error("No results to visualize");
      const controller = new AbortController();
      vizAbortRef.current = controller;
      return api.suggestVisualization(
        {
          columns: result.data.columns.map((c) => ({
            name: c.name,
            friendly_type: c.friendly_type,
          })),
          row_count: result.data.rows.length,
        },
        controller.signal,
      );
    },
    onSuccess: (res: SuggestVisualizationResponse) => {
      if (res.status === "ok") {
        setPendingAutoCreate({ type: res.type, name: `✨ ${res.title}`, options: res.options });
      } else {
        setAutoVizError(aiErrorMessage(res));
      }
    },
    onError: (err) => {
      if (err instanceof DOMException && err.name === "AbortError") return;
      setAutoVizError(apiErrorMessage(err, "Failed to suggest a visualization"));
    },
  });

  function handleAutoVisualize() {
    setAutoVizError(null);
    suggestVizMutation.mutate();
  }

  // Abort in-flight AI calls on unmount: wasted LLM request and handlers touching a torn-down tree.
  useEffect(() => {
    return () => {
      vizAbortRef.current?.abort();
    };
  }, []);

  function handleApplyGeneratedSql() {
    if (!aiGenerated) return;
    if (!isScaffoldSql(sql) && (bufferDiffersFromSaved || queryId == null)) {
      setApplyConfirmOpen(true);
    } else {
      applyGeneratedSql(aiGenerated.sql);
    }
  }

  const isExecuting = executeMutation.isPending || (activeJobId != null && !job.isDone);
  const isStale = result !== null && resultForSql !== null && resultForSql.trim() !== sql.trim();
  // Live editor vs *saved* server state (unlike `isStale`); parameters are excluded as they
  // autosave. Gates the confirm on applying AI-generated SQL.
  const bufferDiffersFromSaved =
    queryId != null &&
    existingQueryQuery.data != null &&
    (sql.trim() !== existingQueryQuery.data.query.trim() ||
      name !== existingQueryQuery.data.name ||
      description !== (existingQueryQuery.data.description ?? "") ||
      dataSourceId !== existingQueryQuery.data.data_source_id);

  // Refetch and rehydrate every field (after a 409 or version revert), since the id-keyed hydrate
  // will not refire; `autosave.markClean()` resets the saved baseline.
  async function handleReload() {
    const { data, error } = await existingQueryQuery.refetch();
    if (error || !data) {
      setSaveError("Couldn't reload this query. Check your connection and try again.");
      return;
    }
    hydrateBuffer(data);
    setActiveJobId(null);
    autosave.markClean();
    setConflict(false);
    autosave.clearConflict();
  }

  // A new query with real SQL but no name exists only in memory; nothing else protects it.
  const hasUnsavedNewDraft = isNew && !canPersist && !isScaffoldSql(sql);

  // Only unacknowledged edits (in flight, offline, failed, conflict) are at risk; the debounce
  // window is covered by the unmount flush.
  const shouldBlock = autosave.hasUnpersistedEdits || conflict || hasUnsavedNewDraft;
  // Mirrors `shouldBlock` for call-time reads: `useBlocker`/`beforeunload` re-subscribe only on
  // effect flush, which lags a state-driven navigate().
  const shouldBlockRef = useRef(shouldBlock);
  shouldBlockRef.current = shouldBlock;

  // A function so react-router registers ONE stable blocker; refs keep it current mid-navigation.
  const shouldBlockNavigation = useCallback<BlockerFunction>(({ nextLocation }) => {
    // A row exists for this draft: the URL swap from `persistBuffer` loses nothing, but other
    // destinations in that window still need the guard.
    if (selfCreatedIdRef.current != null && nextLocation.pathname === `/queries/${selfCreatedIdRef.current}`) {
      return false;
    }
    return shouldBlockRef.current;
  }, []);
  const blocker = useBlocker(shouldBlockNavigation);

  useEffect(() => {
    function handleBeforeUnload(event: BeforeUnloadEvent) {
      if (selfCreatedIdRef.current != null) return;
      // `shouldBlock` misses an edit in the ~1s debounce window; closing the tab leaves no time for
      // the unmount flush.
      if (!shouldBlockRef.current && !autosave.hasUnflushedChanges()) return;
      event.preventDefault();
      event.returnValue = "";
    }
    window.addEventListener("beforeunload", handleBeforeUnload);
    return () => window.removeEventListener("beforeunload", handleBeforeUnload);
    // eslint-disable-next-line react-hooks/exhaustive-deps -- registered once; reads live refs
  }, []);

  if (queryId != null && existingQueryQuery.isLoading) {
    return <PageLoader />;
  }

  // Excludes a refetch failure after first load, which keeps the editor and its inline error.
  if (queryId != null && existingQueryQuery.isError && !existingQueryQuery.data) {
    const isNotFound = existingQueryQuery.error instanceof ApiError && existingQueryQuery.error.status === 404;
    return (
      <div className="flex flex-col gap-4">
        <BackButton to="/queries" label="Back to queries" />
        <div className="flex min-h-[50vh] flex-col items-center justify-center gap-3 text-center">
          <h1 className="text-2xl font-bold text-foreground">
            {isNotFound ? "Query not found" : "Failed to load query"}
          </h1>
          <p className="text-sm text-muted-foreground">
            {isNotFound
              ? "This query doesn't exist."
              : apiErrorMessage(existingQueryQuery.error, "Failed to load query")}
          </p>
        </div>
      </div>
    );
  }

  const isFavorite = existingQueryQuery.data?.is_favorite ?? false;
  const state = existingQueryQuery.data
    ? effectiveState(existingQueryQuery.data, canEditExisting ? "editor" : "viewer")
    : null;

  const primaryActions: ActionItem[] = [
    {
      key: "execute",
      label: "Execute",
      pendingLabel: "Running…",
      pending: isExecuting,
      disabled: !canExecute || dataSourceId == null,
      title: !canExecute && executeBlockedReason ? EXECUTE_BLOCKED_COPY[executeBlockedReason] : undefined,
      onSelect: () => executeMutation.mutate(),
    },
  ];
  if (stopVisible) {
    primaryActions.push({
      key: "cancel",
      label: "Stop",
      icon: CircleStop,
      variant: "outline",
      pending: cancelMutation.isPending || cancelMutation.isSuccess,
      pendingLabel: "Stopping…",
      title: "Stop this run. Results are discarded.",
      onSelect: () => cancelMutation.mutate(),
    });
  }

  const saved = queryId != null && existingQueryQuery.data != null;
  const canEditSaved = saved && canEditExisting;
  const lifecycle = state?.lifecycle;
  const secondaryActions: ActionItem[] = [];
  if (queryId != null) {
    secondaryActions.push({
      key: "history",
      label: "History",
      icon: History,
      group: 1,
      onSelect: () => setHistoryOpen(true),
    });
  }
  if (queryId != null && canCreate) {
    secondaryActions.push({
      key: "fork",
      label: "Fork",
      icon: GitFork,
      pending: forkMutation.isPending,
      pendingLabel: "Forking…",
      group: 1,
      onSelect: () => forkMutation.mutate(),
    });
  }
  if (saved && canManageSharing) {
    secondaryActions.push({
      key: "share",
      label: "Share",
      icon: Share2,
      group: 3,
      onSelect: () => setShareOpen(true),
    });
  }
  if (canEditSaved && lifecycle !== "published_clean") {
    secondaryActions.push({
      key: "publish",
      label: lifecycle === "draft" ? "Publish" : "Publish changes",
      group: 3,
      onSelect: () => setPublishConfirmOpen(true),
    });
  }
  if (canEditSaved && (lifecycle === "published_clean" || lifecycle === "published_pending")) {
    secondaryActions.push({
      key: "unpublish",
      label: "Unpublish",
      pending: unpublishMutation.isPending,
      pendingLabel: "Unpublishing…",
      group: 3,
      onSelect: () => setUnpublishConfirmOpen(true),
    });
  }
  if (canEditSaved && lifecycle === "published_pending") {
    secondaryActions.push({
      key: "discard-changes",
      label: "Discard changes",
      destructive: true,
      group: 3,
      onSelect: () => setDiscardConfirmOpen(true),
    });
  }
  if (canEditSaved && existingQueryQuery.data) {
    secondaryActions.push({
      key: "archive",
      label: existingQueryQuery.data.is_archived ? "Unarchive" : "Archive",
      icon: existingQueryQuery.data.is_archived ? ArchiveRestore : Archive,
      pending: toggleArchivedMutation.isPending,
      pendingLabel: existingQueryQuery.data.is_archived ? "Unarchiving…" : "Archiving…",
      destructive: true,
      group: 4,
      onSelect: () => toggleArchivedMutation.mutate(),
    });
  }
  if (saved) {
    secondaryActions.push({
      key: "star",
      label: isFavorite ? "Unstar" : "Star",
      iconOnly: true,
      pending: starMutation.isPending,
      group: 1,
      icon: (p) => (
        <Star {...p} className={cn(p.className, isFavorite && "fill-yellow-400 text-yellow-400")} />
      ),
      onSelect: () => starMutation.mutate(),
    });
  }

  const { Icon: LastRunIcon, colorClassName: lastRunColorClassName } = executionStatusIcon(
    existingQueryQuery.data?.last_execution_status ?? "New",
  );
  const lastRunValue = existingQueryQuery.data?.last_execution_at
    ? `${existingQueryQuery.data.last_execution_status}, ${new Date(existingQueryQuery.data.last_execution_at).toLocaleString()}`
    : "Never";
  const createdAt = existingQueryQuery.data
    ? new Date(existingQueryQuery.data.created_at).toLocaleString()
    : "";
  const updatedAt = existingQueryQuery.data
    ? new Date(existingQueryQuery.data.updated_at).toLocaleString()
    : "";
  const publishCopy =
    existingQueryQuery.data &&
    publishDialogCopy("query", existingQueryQuery.data.visibility, !existingQueryQuery.data.published);
  const currentTags = existingQueryQuery.data?.tags ?? [];
  const tagSuggestions = (tagsQuery.data?.tags ?? []).filter((t) => !currentTags.includes(t));

  return (
    <div className="flex flex-col gap-4">
      {/* The name Input has no visible label; assistive tech needs a real heading. */}
      <h1 className="sr-only">{name || "Untitled query"}</h1>
      <BackButton to="/queries" label="Back to queries" />
      <div className="@container/qhead flex flex-col gap-2">
        <div className="flex min-w-0 flex-wrap items-center gap-3">
          <Label htmlFor="query-name" className="sr-only">
            Query name
          </Label>
          <Input
            id="query-name"
            className="w-full max-w-72 min-w-48"
            placeholder="Untitled query"
            value={name}
            onChange={(e) => {
              setName(e.target.value);
              setNameTouched(true);
            }}
          />
          {dataSourcesQuery.data && (
            <DataSourcePicker
              dataSources={dataSourcesQuery.data.data_sources}
              value={dataSourceId}
              onChange={setDataSourceId}
            />
          )}
          {existingQueryQuery.data && state && (
            <>
              <div className="flex flex-wrap items-center gap-1.5">
                <Badge variant={state.badge === "Draft" ? "secondary" : "default"}>{state.badge}</Badge>
                {state.pending && <UnpublishedBadge />}
                {existingQueryQuery.data.is_archived && (
                  <Badge variant="outline">Archived</Badge>
                )}
                {queryId != null && (
                  <FreshnessChip
                    queryId={queryId}
                    lifecycle={state.lifecycle}
                    onOpenFailed={(runId) => {
                      setScheduleExpanded(true);
                      setScheduleFocusRunId(runId);
                    }}
                  />
                )}
              </div>
              <p className="w-full text-xs text-muted-foreground @min-[48rem]/qhead:w-auto">
                {state.hint} <DocsLink topic="draftVsPublished" label="drafts and published versions" />
              </p>
            </>
          )}
          {existingQueryQuery.data && (
            <div className="ml-auto flex items-center gap-1">
              <Tooltip>
                <TooltipTrigger asChild>
                  <button
                    type="button"
                    aria-label={`Owner: ${existingQueryQuery.data.owner_name ?? "—"}`}
                    className="cursor-pointer rounded-full focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
                  >
                    <Avatar className="size-6">
                      <AvatarFallback className="text-[10px]">
                        {initials(existingQueryQuery.data.owner_name ?? "—")}
                      </AvatarFallback>
                    </Avatar>
                  </button>
                </TooltipTrigger>
                <TooltipContent className="w-48 overflow-hidden rounded-lg border bg-popover p-0 text-popover-foreground shadow-md">
                  <MetaTooltipCard
                    title="Owner"
                    rows={[
                      {
                        key: "owner",
                        icon: <User className="size-3.5 text-muted-foreground" />,
                        value: existingQueryQuery.data.owner_name ?? "—",
                      },
                    ]}
                  />
                </TooltipContent>
              </Tooltip>
              <Tooltip>
                <TooltipTrigger asChild>
                  <button
                    type="button"
                    aria-label="Query details"
                    className="cursor-pointer inline-flex items-center rounded p-1 text-muted-foreground hover:bg-accent focus-visible:bg-accent focus-visible:outline-none"
                  >
                    <Info className="size-3.5" />
                  </button>
                </TooltipTrigger>
                <TooltipContent className="w-56 overflow-hidden rounded-lg border bg-popover p-0 text-popover-foreground shadow-md">
                  <MetaTooltipCard
                    title="Query info"
                    rows={[
                      {
                        key: "last-run",
                        icon: <LastRunIcon className={cn("size-3.5", lastRunColorClassName)} />,
                        label: "Last run",
                        value: lastRunValue,
                      },
                      {
                        key: "created",
                        icon: <CalendarPlus className="size-3.5 text-muted-foreground" />,
                        label: "Created",
                        value: createdAt,
                      },
                      {
                        key: "updated",
                        icon: <CalendarClock className="size-3.5 text-muted-foreground" />,
                        label: "Updated",
                        value: updatedAt,
                      },
                    ]}
                  />
                </TooltipContent>
              </Tooltip>
            </div>
          )}
        </div>
        <div className="flex min-w-0 items-center gap-3">
          <Label htmlFor="query-description" className="sr-only">
            Description
          </Label>
          <Input
            id="query-description"
            className="w-full max-w-2xl"
            placeholder="Add a description…"
            value={description}
            onChange={(e) => setDescription(e.target.value)}
          />
        </div>
        {queryId != null && existingQueryQuery.data && (
          <TagEditor
            tags={currentTags}
            editable={canEditExisting}
            pending={tagsMutation.isPending}
            onChange={(tags) => tagsMutation.mutate(tags)}
            suggestions={{ id: "query-tag-suggestions", tags: tagSuggestions }}
          />
        )}
        <div className="flex min-w-0 items-center gap-3">
          {isNew ? (
            <AutosaveIndicator
              status={autosave.status}
              onRetry={autosave.retry}
              idleHint={
                canPersist
                  ? "Will save automatically as you keep editing."
                  : "Add SQL and a name — or run it — to save this query."
              }
            />
          ) : (
            canPersist && <AutosaveIndicator status={autosave.status} onRetry={autosave.retry} />
          )}
          <ActionBar
            primary={primaryActions}
            secondary={secondaryActions}
            collapseAt="56rem"
            menuOrder={[
              "publish",
              "unpublish",
              "discard-changes",
              "share",
              "history",
              "fork",
              "star",
              "archive",
            ]}
          />
        </div>
      </div>

      {queryId != null && (
        <VersionHistoryPanel
          queryId={queryId}
          workingVersion={existingQueryQuery.data?.working_version ?? 0}
          isCurrentlyPublished={existingQueryQuery.data?.published ?? false}
          onReverted={(message) => {
            handleReload();
            setHistoryFeedback(message);
          }}
          onConflict={() => {
            setHistoryOpen(false);
            setConflict(true);
          }}
          open={historyOpen}
          onOpenChange={setHistoryOpen}
        />
      )}
      {queryId != null && existingQueryQuery.data && (
        <ShareDialog
          objectType="query"
          objectId={queryId}
          ownerId={existingQueryQuery.data.user_id}
          visibility={existingQueryQuery.data.visibility}
          open={shareOpen}
          canManage={canManageSharing}
          onClose={() => setShareOpen(false)}
          onChanged={() => existingQueryQuery.refetch()}
        />
      )}
      {existingQueryQuery.data && publishCopy && (
        <Dialog open={publishConfirmOpen} onOpenChange={setPublishConfirmOpen}>
          <DialogContent>
            <DialogHeader>
              <DialogTitle>{publishCopy.title}</DialogTitle>
            </DialogHeader>
            <p className="text-sm text-muted-foreground">{publishCopy.body}</p>
            {existingQueryQuery.data.last_execution_status !== "Success" && (
              <p className="text-sm text-warning-foreground">
                You haven't run this query successfully yet — you can still publish it.
              </p>
            )}
            <DialogFooter>
              <Button variant="outline" onClick={() => setPublishConfirmOpen(false)}>
                Cancel
              </Button>
              <Button onClick={() => publishMutation.mutate()} disabled={publishMutation.isPending}>
                {publishMutation.isPending ? "Publishing…" : publishCopy.confirmLabel}
              </Button>
            </DialogFooter>
          </DialogContent>
        </Dialog>
      )}
      <Dialog open={discardConfirmOpen} onOpenChange={setDiscardConfirmOpen}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>Discard your changes?</DialogTitle>
          </DialogHeader>
          <p className="text-sm text-muted-foreground">
            Your unpublished edits will be replaced with the published version. This can't be undone.
          </p>
          <DialogFooter>
            <Button variant="outline" onClick={() => setDiscardConfirmOpen(false)}>
              Cancel
            </Button>
            <Button
              variant="destructive"
              onClick={() => discardMutation.mutate()}
              disabled={discardMutation.isPending}
            >
              {discardMutation.isPending ? "Discarding…" : "Discard changes"}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      <ConfirmDialog
        open={unpublishConfirmOpen}
        onOpenChange={setUnpublishConfirmOpen}
        title="Unpublish this query?"
        description="It goes back to draft. Only you and the people you've shared it with will see it."
        confirmLabel="Unpublish"
        confirmPending={unpublishMutation.isPending}
        onConfirm={() => unpublishMutation.mutate()}
        body={
          <ul className="list-disc space-y-1 pl-5 text-sm text-muted-foreground">
            <li>Your workspace loses access until you publish again.</li>
            <li>
              Public links and embeds stop working. They keep their tokens and start working
              again when you publish.
            </li>
            <li>Scheduled refreshes stop running.</li>
            {unpublishConsumersQuery.isPending ? (
              <li>Monitors on this query stop checking and keep their last state.</li>
            ) : (
              (unpublishConsumersQuery.data?.monitor_count ?? 0) > 0 && (
                <li>
                  {unpublishConsumersQuery.data!.monitor_count} monitor
                  {unpublishConsumersQuery.data!.monitor_count === 1 ? "" : "s"} on this query stop
                  checking and keep their last state.
                </li>
              )
            )}
          </ul>
        }
      />

      {conflict && (
        <div className="flex items-center justify-between gap-3 rounded-md border border-border bg-muted/50 px-3 py-2 text-sm">
          <span>This query was changed by someone else. Reload to get their version.</span>
          <Button variant="outline" size="sm" onClick={handleReload}>
            Reload
          </Button>
        </div>
      )}

      {historyFeedback && <p className="text-sm text-muted-foreground">{historyFeedback}</p>}

      {saveError && <p className="text-sm text-destructive">{saveError}</p>}

      <div className="flex items-start gap-4">
        <aside className="hidden lg:block w-64 shrink-0 max-h-[max(24rem,60dvh)] overflow-auto rounded-md border border-border bg-card p-3">
          <SchemaBrowser dataSourceId={dataSourceId} onInsert={handleInsert} />
        </aside>

        <Sheet open={schemaOpen} onOpenChange={setSchemaOpen}>
          <SheetContent side="left">
            <SheetTitle className="sr-only">Schema</SheetTitle>
            <SchemaBrowser
              dataSourceId={dataSourceId}
              onInsert={handleInsert}
              className="p-3 overflow-auto"
            />
          </SheetContent>
        </Sheet>

        <div className="flex min-w-0 flex-1 flex-col gap-4">
          <div className="flex min-w-0 flex-col gap-2 sm:flex-row sm:items-center sm:justify-between">
            <div className="flex min-w-0 items-center gap-1 overflow-x-auto custom-scroll border-b border-border">
              {EDITOR_TABS.map(([tab, label]) => (
                <button
                  key={tab}
                  type="button"
                  className={`cursor-pointer rounded-t px-3 py-1.5 text-sm font-medium whitespace-nowrap transition-colors ${
                    editorTab === tab
                      ? "border-b-2 border-primary text-foreground"
                      : "text-muted-foreground hover:text-foreground"
                  }`}
                  onClick={() => setEditorTab(tab)}
                >
                  {label}
                </button>
              ))}
              <Button
                variant="outline"
                size="icon-sm"
                aria-label="Browse schema"
                className="lg:hidden ml-1 shrink-0"
                onClick={() => setSchemaOpen(true)}
              >
                <Database className="size-3.5" />
              </Button>
            </div>
            {editorTab === "sql" && has("view_snippets") && (
              <div className="flex justify-end sm:contents">
                <SnippetMenu
                  onInsert={(body, trigger) => editorRef.current?.insertSnippet(body, trigger)}
                />
              </div>
            )}
          </div>

          {editorTab === "sql" && !canSave && (
            <p className="rounded-md bg-muted px-3 py-2 text-sm text-muted-foreground">
              You don't have permission to edit this query. Viewing only.
            </p>
          )}
          {editorTab === "sql" && (
            <div className="h-64 min-h-32 shrink-0 resize-y overflow-hidden rounded-md border border-border shadow-sm">
              <SqlEditor
                ref={editorRef}
                value={sql}
                onChange={setSql}
                schema={schemaQuery.data?.schema}
                snippets={snippetsQuery.data}
                className="h-full"
                readOnly={!canSave}
              />
            </div>
          )}

          {editorTab === "ai" && (
            <div
              ref={askPanelRef}
              className={`min-h-64 shrink-0 rounded-md border border-border shadow-sm ${
                askStarted ? "space-y-2 p-3" : "space-y-3 p-4"
              }`}
            >
              {dataSourceId == null ? (
                <p className="text-sm text-muted-foreground">
                  Select a data source first — AI needs its schema to write SQL.
                </p>
              ) : !aiEnabled ? (
                has("admin") ? (
                  <p className="text-sm text-muted-foreground">
                    AI SQL generation isn&apos;t configured yet. Add a Gemini API key in{" "}
                    <AiSettingsLink isAdmin /> to enable it.
                  </p>
                ) : (
                  <p className="text-sm text-muted-foreground">
                    AI SQL generation isn&apos;t configured. Ask an admin to add a Gemini API key
                    in AI Settings.
                  </p>
                )
              ) : (
                <>
                  <Textarea
                    aria-label="Ask AI a question about your data"
                    placeholder="e.g., 'How many users signed up last week from the US?' or 'Show me the top 10 products by revenue'"
                    value={aiQuestion}
                    onChange={(e) => setAiQuestion(e.target.value)}
                    disabled={askRunning}
                    rows={askStarted ? 1 : 3}
                    className={askStarted ? "h-8 min-h-0 resize-none py-1.5" : undefined}
                  />
                  {!askStarted && <AskDisclosure isAdmin={has("admin")} />}

                  {!askStarted && isScaffoldSql(sql) && !aiGenerated && (
                    <div className="flex flex-wrap gap-2">
                      {SUGGESTED_PROMPTS.map((prompt) => (
                        <button
                          key={prompt}
                          type="button"
                          className="cursor-pointer rounded-full border border-border px-3 py-1 text-xs text-muted-foreground transition-colors hover:bg-accent hover:text-accent-foreground"
                          onClick={() => setAiQuestion(prompt)}
                        >
                          {prompt}
                        </button>
                      ))}
                    </div>
                  )}

                  {!askRunning && askState.phase !== "done" && (
                    <div className="flex items-center gap-2">
                      <Button onClick={handleGenerateSql} disabled={!aiQuestion.trim()}>
                        Generate SQL
                      </Button>
                      {askState.phase === "failed" && (
                        <Button variant="outline" onClick={ask.reset}>
                          Discard
                        </Button>
                      )}
                    </div>
                  )}

                  <AskTimeline state={askState} onCancel={ask.cancel} key={askState.runId} />

                  {(askRunning || aiGenerated) && askState.latestSql && (
                    <div className="flex flex-col gap-2">
                      {aiGenerated?.explanation && askState.succeeded && (
                        <p
                          className="line-clamp-1 text-sm text-muted-foreground"
                          title={aiGenerated.explanation}
                        >
                          {aiGenerated.explanation}
                        </p>
                      )}
                      <GeneratedSqlView sql={askState.latestSql} />
                      {askState.preview && <AskPreviewGrid preview={askState.preview} />}
                      {askProblem && <ExhaustedWarning problem={askProblem} />}
                      {aiGenerated && (
                        <div className="flex flex-wrap items-center gap-2">
                          <Button size="sm" onClick={handleApplyGeneratedSql}>
                            Apply
                          </Button>
                          <Button size="sm" variant="outline" onClick={handleGenerateSql}>
                            Regenerate
                          </Button>
                          <Button size="sm" variant="outline" onClick={ask.reset}>
                            Discard
                          </Button>
                        </div>
                      )}
                    </div>
                  )}
                  {askStarted && <AskDisclosure isAdmin={has("admin")} />}
                </>
              )}
            </div>
          )}

          <ParametersBar
            sql={sql}
            definitions={parameterDefinitions}
            values={parameterValues}
            onValuesChange={setParameterValues}
            onDefinitionsChange={(defs) =>
              persistParameterDefinitions(defs).catch((err) => {
                setSaveError(apiErrorMessage(err, "Failed to save parameter"));
                throw err;
              })
            }
            queryId={queryId}
          />

          <div data-testid="query-results-pane">
            {executeBlockedReason && (
              <div className="mb-3 rounded-md border border-warning/30 bg-warning/10 px-3 py-2 text-sm text-warning-foreground">
                {EXECUTE_BLOCKED_COPY[executeBlockedReason]}
              </div>
            )}
            {dsAccessDenied && (
              <div className="mb-3 rounded-md border border-warning/30 bg-warning/10 px-3 py-2 text-sm text-warning-foreground">
                You can open this query, but you don't have access to its data source, so it
                can't run. Ask an admin for access to the data source.
              </div>
            )}
            {execError && !result && (
              <div className="mb-3">
                <p className="text-sm text-destructive">{execError}</p>
              </div>
            )}
            {execError && result && (
              <p className="mb-3 rounded-md border border-warning/30 bg-warning/10 px-3 py-2 text-sm text-warning-foreground">
                ⚠ {execError}
              </p>
            )}

            {activeJobId && !job.isSuccess && (
              <ExecuteStatusBanner
                isPending={job.isPending}
                isStarted={job.isStarted}
                isFailure={job.isFailure}
                isCancelled={job.isCancelled}
                gaveUp={job.gaveUp}
                error={job.error}
              />
            )}

            {fixTriggerError != null && aiEnabled && dataSourceId != null && (
              <div className="mb-3 mt-2 space-y-2" role="status">
                {askRunning && askState.source === "fix" ? (
                  <button
                    type="button"
                    className="text-sm text-primary underline"
                    onClick={() => setEditorTab("ai")}
                  >
                    Fixing in Ask with AI →
                  </button>
                ) : (
                  <div className="flex items-center gap-2">
                    <Button onClick={handleFixQuery}>✨ Fix with AI</Button>
                    <AskDisclosure isAdmin={has("admin")} />
                  </div>
                )}
              </div>
            )}

            {(job.isPending || job.isStarted) && !result && (
              <div className="mt-2 space-y-2 animate-pulse">
                {[80, 60, 72, 55, 68].map((w, i) => (
                  <div key={i} className="h-8 rounded-md bg-muted" style={{ width: `${w}%` }} />
                ))}
              </div>
            )}

            {result && (
              <>
                {isStale && (
                  <div className="mb-3 flex flex-wrap items-center justify-between gap-3 rounded-md border border-warning/30 bg-warning/10 px-3 py-2 text-sm text-warning-foreground">
                    <span>
                      These results came from an earlier version of this SQL. Everything here
                      still works — export and visualizations use the results shown. Re-run to
                      match the editor.
                    </span>
                    <Button
                      variant="outline"
                      size="sm"
                      onClick={() => executeMutation.mutate()}
                      disabled={isExecuting || dataSourceId == null}
                    >
                      Re-run
                    </Button>
                  </div>
                )}
                <div
                  className={cn(
                    "animate-fade-in space-y-2",
                    isStale && "border-l-2 border-warning/60 pl-3",
                  )}
                >
                  <div className="flex items-center justify-between">
                    <p
                      className="flex min-w-0 flex-wrap items-center gap-x-2 gap-y-1 text-sm text-muted-foreground"
                      title={exactTs(Date.parse(result.retrieved_at))}
                    >
                      {isStale && (
                        <Badge
                          variant="outline"
                          className="gap-1.5 border-warning/30 bg-warning/10 text-warning-foreground"
                        >
                          <span className="size-1.5 rounded-full bg-warning" />
                          Earlier SQL
                        </Badge>
                      )}
                      {result.data.rows.length} row{result.data.rows.length === 1 ? "" : "s"} ·{" "}
                      {(result.data.runtime_seconds ?? result.runtime).toFixed(3)}s · Updated{" "}
                      {timeAgo(Date.parse(result.retrieved_at), now)}
                      {hasResultFootnote() && (
                        <ResultFootnote retrievedAt={result.retrieved_at} placement="editor" />
                      )}
                    </p>
                    <div className="flex items-center gap-2">
                      {queryId != null &&
                        (suggestVizMutation.isPending ? (
                          <Button variant="outline" size="sm" disabled>
                            <Loader size={16} />
                            Visualizing…
                          </Button>
                        ) : (
                          <Button variant="outline" size="sm" onClick={handleAutoVisualize}>
                            ✨ Auto Visualize
                          </Button>
                        ))}
                      <ExportMenu resultId={result.id} />
                    </div>
                  </div>
                  {queryId != null && autoVizError && (
                    <div className="flex items-center justify-between gap-3 rounded-md border border-destructive/30 bg-destructive/10 px-3 py-2 text-sm text-destructive">
                      <span>{autoVizError}</span>
                      <Button variant="outline" size="sm" onClick={handleAutoVisualize}>
                        Try again
                      </Button>
                    </div>
                  )}
                  {queryId != null ? (
                    <VizPanel
                      queryId={queryId}
                      result={result.data}
                      queryVisibility={existingQueryQuery.data?.visibility}
                      autoCreateRequest={pendingAutoCreate}
                      onAutoCreateHandled={() => setPendingAutoCreate(null)}
                    />
                  ) : (
                    <ResultsTable data={result.data} />
                  )}
                </div>
              </>
            )}

            {!result &&
              !activeJobId &&
              !execError &&
              !dsAccessDenied &&
              !executeBlockedReason &&
              !(queryId != null && latestResultQuery.isLoading) &&
              (savedVisualizations.length > 0 ? (
                <p className="text-sm text-muted-foreground">
                  This query has {savedVisualizations.length} saved visualization
                  {savedVisualizations.length === 1 ? "" : "s"} —{" "}
                  {savedVisualizations.map((v) => v.name).join(", ")}. Run the query to see them.
                </p>
              ) : (
                <p className="text-sm text-muted-foreground">Run the query to see results.</p>
              ))}
          </div>
        </div>
      </div>

      {queryId != null && existingQueryQuery.data && (
        <ScheduleSection
          entityId={queryId}
          flavor={QUERY_FLAVOR}
          canEdit={canEditExisting}
          published={existingQueryQuery.data.published}
          lifecycle={state?.lifecycle ?? "draft"}
          onPublishClick={() => setPublishConfirmOpen(true)}
          expanded={scheduleExpanded}
          onExpandedChange={setScheduleExpanded}
          focusRunId={scheduleFocusRunId}
        />
      )}

      {queryId != null && existingQueryQuery.data && (
        <div className="shrink-0 space-y-3 rounded-md border border-border bg-card p-3">
          <div className="flex items-center justify-between">
            <h2 className="text-sm font-medium">Monitors</h2>
            <div className="flex items-center gap-2">
              {showSuggestMonitors && (
                <Button
                  size="sm"
                  variant="outline"
                  disabled={suggestDisabledReason != null}
                  title={suggestDisabledReason ?? undefined}
                  onClick={() => setSuggestMonitorsOpen(true)}
                  data-testid="monitor-suggest-open"
                >
                  <Sparkles className="size-3.5" />
                  Suggest with AI
                </Button>
              )}
              <Button
                size="sm"
                variant="outline"
                disabled={!canCreateMonitor}
                title={canCreateMonitor ? undefined : "You don't have permission to create monitors"}
                onClick={() => navigate(`/monitors/new?query_id=${queryId}`)}
              >
                Add monitor
              </Button>
            </div>
          </div>
          {showSuggestMonitors && suggestDisabledReason != null && (
            <p className="text-xs text-muted-foreground" data-testid="monitor-suggest-disabled-reason">
              {suggestDisabledReason}
              {!existingQueryQuery.data.published && canEditExisting && (
                <>
                  {" "}
                  <button type="button" className="text-primary underline" onClick={() => setPublishConfirmOpen(true)}>
                    Publish
                  </button>
                </>
              )}
            </p>
          )}
          {showSuggestMonitors && (
            <SuggestMonitorsPane
              queryId={queryId}
              queryName={existingQueryQuery.data.name}
              hasUnpublishedChanges={existingQueryQuery.data.has_unpublished_changes}
              open={suggestMonitorsOpen}
              onOpenChange={setSuggestMonitorsOpen}
            />
          )}
          {monitorsForQueryQuery.isLoading && <Loader size={16} announce />}
          {monitorsForQueryQuery.data && monitorsForQueryQuery.data.length === 0 && (
            <EmptyState
              icon={BellRing}
              iconClassName="bg-warning/10 text-warning"
              title="No monitors watch this query"
              description="A monitor checks this query on a schedule and alerts you when the data looks wrong."
            />
          )}
          {monitorsForQueryQuery.data && monitorsForQueryQuery.data.length > 0 && (
            <ul className="space-y-2">
              {monitorsForQueryQuery.data.map((m) => (
                <li key={m.id} className="flex items-center justify-between gap-2 text-sm">
                  <Link to={`/monitors/${m.id}`} className="text-primary underline">
                    {m.name}
                  </Link>
                  <div className="flex items-center gap-2">
                    <span className="text-xs text-muted-foreground">
                      {monitorScheduleState(m, scheduleForMonitor(m.id))}
                    </span>
                    <StateBadge state={m.state} />
                  </div>
                </li>
              ))}
            </ul>
          )}
        </div>
      )}

      <Dialog open={applyConfirmOpen} onOpenChange={setApplyConfirmOpen}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>Replace your current SQL with the generated query?</DialogTitle>
          </DialogHeader>
          <DialogFooter>
            <Button variant="outline" onClick={() => setApplyConfirmOpen(false)}>
              Cancel
            </Button>
            <Button onClick={() => aiGenerated && applyGeneratedSql(aiGenerated.sql)}>
              Replace
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      <ConfirmDialog
        open={blocker.state === "blocked"}
        onOpenChange={(open) => {
          if (!open && blocker.state === "blocked") blocker.reset();
        }}
        title="Leave without saving?"
        description={
          autosave.hasUnpersistedEdits || conflict
            ? "Your latest edits haven't saved yet. Leave anyway?"
            : "This draft hasn't been saved — autosave starts once you name the query and pick a data source. Leave now and your SQL is discarded."
        }
        confirmLabel="Leave"
        destructive
        onConfirm={() => {
          if (blocker.state === "blocked") blocker.proceed();
        }}
      />
    </div>
  );
}
