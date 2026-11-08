import { memo, useEffect, useMemo, useRef, useState } from "react";
import { keepPreviousData, useQuery } from "@tanstack/react-query";
import { GripVertical, X } from "lucide-react";
import ReactMarkdown from "react-markdown";
import { Link } from "react-router";
import { Button } from "@/components/ui/button";
import { Loader } from "@/components/ui/loader";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { ApiError, api, apiErrorMessage } from "@/lib/api";
import { usePollJob } from "@/lib/jobs";
import { ErrorBoundary } from "@/components/ErrorBoundary";
import { getViz, toVizData } from "@/components/visualizations/registry";
import { TileFreshnessDot } from "./TileFreshnessDot";
import { MonitorWidget, computeMonitorCardMeta, type MonitorCardMeta } from "./MonitorWidget";
import type { Widget, ResultData, ParamValue } from "@/types/api";
import type { QueryResultData } from "@/components/visualizations/registry";

interface Props {
  widget: Widget;
  editMode: boolean;
  /** Gates the drag affordances only; false where tiles can't be moved (phone stack). */
  draggable?: boolean;
  onDelete: () => void;
  dashboardParams?: Record<string, ParamValue>;
  refreshKey?: number;
  /** Public/embed: age shown inline, freshness only for scheduled queries. */
  publicView?: boolean;
  /** Rings the tile briefly to mark a freshly added widget. */
  highlight?: boolean;
}

function isCompleteParamValue(value: ParamValue | undefined): boolean {
  if (value == null) return false;
  if (Array.isArray(value)) return value.length > 0;
  if (typeof value === "object") return Boolean(value.start) && Boolean(value.end);
  if (typeof value === "number") return true;
  return value !== "";
}

/** Narrows the dashboard param bag to the tokens this widget's SQL uses, only if each has a
 * complete value; else `undefined` so a partial set (half-typed range, unrelated param) never
 * reaches `execute`. */
export function resolveWidgetParams(
  tokens: string[] | undefined,
  dashboardParams: Record<string, ParamValue> | undefined,
): Record<string, ParamValue> | undefined {
  if (!tokens || tokens.length === 0 || !dashboardParams) return undefined;

  const resolved: Record<string, ParamValue> = {};
  for (const token of tokens) {
    const value = dashboardParams[token];
    if (!isCompleteParamValue(value)) return undefined;
    resolved[token] = value;
  }
  return resolved;
}

const VizWidget = memo(function VizWidget({
  widget,
  dashboardParams,
  refreshKey,
}: {
  widget: Widget;
  dashboardParams?: Record<string, ParamValue>;
  refreshKey?: number;
}) {
  const viz = widget.visualization;
  const preloaded = widget.query_result_data;
  const queryId = viz?.query?.id ?? viz?.query_id;
  // Public embeds without a real backing query use 0 as a placeholder id, which is not fetchable.
  const hasQueryId = queryId != null && queryId > 0;
  // A restricted fetch would only 404 — the backend already told us why, so skip it.
  const canFetch = hasQueryId && widget.viewer_can_read_data;
  const widgetParams = useMemo(
    () => resolveWidgetParams(viz?.query?.parameter_names, dashboardParams),
    [viz?.query?.parameter_names, dashboardParams],
  );
  const hasParams = widgetParams != null;

  // Disabled when pre-loaded data is available (public dashboard). Fires once per key change; a
  // cache miss returns a job id polled by `usePollJob`.
  const resultQuery = useQuery({
    queryKey: ["query-result", queryId, widgetParams, refreshKey],
    queryFn: () =>
      hasParams
        ? api.executeQuery(queryId!, { parameters: widgetParams })
        : api.getLatestQueryResult(queryId!, true),
    enabled: canFetch && !preloaded,
    // Parameterized runs execute fresh data per param set, so never serve a stale cache hit.
    ...(hasParams ? { staleTime: 0 } : {}),
    // Each tick mints a new key, so keep the replaced entry only long enough to cover the swap, not
    // the default 5 minutes (a long-running wallboard would accumulate results).
    gcTime: 30_000,
    // Without this, a fresh refreshKey has no cache entry and every tile blanks to a spinner each
    // tick.
    placeholderData: keepPreviousData,
  });

  const jobId =
    resultQuery.data && "job" in resultQuery.data && resultQuery.data.job
      ? resultQuery.data.job.id
      : null;
  const job = usePollJob(jobId);
  const [jobResultData, setJobResultData] = useState<ResultData | null>(null);
  const [fetchError, setFetchError] = useState<string | null>(null);

  // A new execute supersedes whatever the previous job resolved to; drop it so a stale tile can't
  // linger.
  useEffect(() => {
    setJobResultData(null);
    setFetchError(null);
  }, [jobId]);

  useEffect(() => {
    if (!job.isSuccess || job.queryResultId == null) return;
    let cancelled = false;
    api
      .getQueryResult(job.queryResultId)
      .then((res) => {
        if (!cancelled) setJobResultData(res.query_result.data);
      })
      .catch((err) => {
        // Without this, a failed fetch leaves jobResultData null with the job terminal-success, so
        // the loader spins forever.
        if (!cancelled) setFetchError(apiErrorMessage(err, "Failed to load data"));
      });
    return () => {
      cancelled = true;
    };
  }, [job.isSuccess, job.queryResultId]);

  const rawQueryResult = (
    resultQuery.data as ({ query_result?: { data: ResultData } } & Record<string, unknown>) | undefined
  )?.query_result;

  const preloadedData = useMemo(() => (preloaded ? toVizData(preloaded) : null), [preloaded]);
  const fetchedData = useMemo(
    () => (rawQueryResult ? toVizData(rawQueryResult.data) : null),
    [rawQueryResult],
  );
  const jobData = useMemo(
    () => (jobResultData ? toVizData(jobResultData) : null),
    [jobResultData],
  );
  const currentData = fetchedData ?? jobData;

  // Keeps the previous numbers across an auto-refresh tick, scoped to this query+params (a
  // different question gets a spinner). Never fed by jobId/jobResultData; the race handling above
  // owns that.
  const [lastGoodData, setLastGoodData] = useState<QueryResultData | null>(null);
  useEffect(() => setLastGoodData(null), [queryId, widgetParams]);
  useEffect(() => {
    if (currentData) setLastGoodData(currentData);
  }, [currentData]);

  if (!viz) return <p className="text-sm text-muted-foreground">No visualization</p>;

  const def = getViz(viz.type);
  if (!def) return <p className="text-sm text-muted-foreground">Unknown viz type: {viz.type}</p>;

  if (preloadedData) {
    return (
      <ErrorBoundary resetKeys={[preloadedData]}>
        <def.Renderer data={preloadedData} options={viz.options} />
      </ErrorBoundary>
    );
  }

  // Checked before the generic no-query-id case: the widget has a query this viewer's data source
  // access doesn't cover.
  if (hasQueryId && !widget.viewer_can_read_data) {
    return (
      <p className="text-sm text-muted-foreground">
        You don&apos;t have access to this widget&apos;s data source.
      </p>
    );
  }

  if (!hasQueryId) {
    return <p className="text-sm text-muted-foreground">No snapshot available yet</p>;
  }

  if (resultQuery.isLoading) {
    return (
      <div className="flex h-full items-center justify-center">
        <Loader size={20} announce />
      </div>
    );
  }

  if (resultQuery.error instanceof ApiError && resultQuery.error.status === 404) {
    return <p className="text-sm text-muted-foreground">No data yet — this query hasn't run.</p>;
  }

  if (resultQuery.isError || !resultQuery.data) {
    return <p className="text-sm text-destructive">Failed to load data</p>;
  }

  // An inline result wins; otherwise use what the polled job resolved to.
  if (currentData) {
    return (
      <ErrorBoundary resetKeys={[currentData]}>
        <def.Renderer data={currentData} options={viz.options} />
      </ErrorBoundary>
    );
  }

  // Keep the last answer on screen while a refresh is pending; terminal failures below still
  // replace it (a stale number shown as current is worse than a spinner).
  if (lastGoodData && jobId != null && !job.isDone && !fetchError) {
    return (
      <div aria-busy="true" className="h-full opacity-70">
        <ErrorBoundary resetKeys={[lastGoodData]}>
          <def.Renderer data={lastGoodData} options={viz.options} />
        </ErrorBoundary>
      </div>
    );
  }

  if (fetchError) {
    return <p className="text-sm text-destructive">{fetchError}</p>;
  }

  if (jobId != null && job.isFailure) {
    return <p className="text-sm text-destructive">{job.error ?? "Failed to load data"}</p>;
  }

  if (jobId != null && job.gaveUp) {
    return <p className="text-sm text-muted-foreground">{job.error}</p>;
  }

  return (
    <div className="flex h-full items-center justify-center">
      <Loader size={20} label="Running…" announce />
    </div>
  );
});

function TextWidget({ text }: { text: string | null }) {
  // react-markdown skips raw HTML by default (no rehype-raw); required since widget text can come
  // from other org members.
  return (
    <div className="h-full overflow-auto text-sm text-foreground [&_h1]:text-lg [&_h1]:font-semibold [&_h2]:text-base [&_h2]:font-semibold [&_h3]:font-semibold [&_p]:my-2 [&_ul]:my-2 [&_ul]:list-disc [&_ul]:pl-5 [&_ol]:my-2 [&_ol]:list-decimal [&_ol]:pl-5 [&_a]:underline [&_code]:rounded [&_code]:bg-muted [&_code]:px-1">
      <ReactMarkdown>{text ?? ""}</ReactMarkdown>
    </div>
  );
}

function TruncatedTitle({ title, href, minW = "min-w-[min(8rem,100%)]" }: { title: string; href?: string; minW?: string }) {
  const ref = useRef<HTMLDivElement>(null);
  const [open, setOpen] = useState(false);
  return (
    <Tooltip
      open={open}
      onOpenChange={(o) => {
        const el = ref.current;
        setOpen(o && el != null && el.scrollHeight > el.clientHeight);
      }}
    >
      <TooltipTrigger asChild>
        {/* The clamp lives on a block wrapper so a link title clamps too (an inline <a> can't). */}
        <div ref={ref} className={`${minW} flex-1 break-words font-serif text-base font-medium leading-snug line-clamp-3`}>
          {href ? (
            <Link
              to={href}
              className="outline-none after:absolute after:inset-0 after:content-[''] hover:underline"
            >
              {title}
            </Link>
          ) : (
            title
          )}
        </div>
      </TooltipTrigger>
      <TooltipContent>{title}</TooltipContent>
    </Tooltip>
  );
}

export function isMonitorWidget(widget: Widget): boolean {
  return widget.visualization_id == null && (widget.monitors != null || (widget.options.monitors?.length ?? 0) > 0);
}

export function widgetTitle(widget: Widget): string {
  if (isMonitorWidget(widget)) return computeMonitorCardMeta(widget).title;
  return widget.visualization?.name ?? (widget.visualization_id ? `Viz #${widget.visualization_id}` : "Text");
}

const CHROME: Record<MonitorCardMeta["treatment"], { root: string; header: string }> = {
  destructive: {
    root: "border-2 border-destructive border-l-8 border-l-destructive bg-destructive/10",
    header: "border-b border-destructive/30 bg-destructive/10",
  },
  warning: {
    root: "border-2 border-warning border-l-8 border-l-warning bg-warning/10",
    header: "border-b border-warning/30 bg-warning/10",
  },
  neutral: { root: "border border-border bg-card", header: "border-b border-border" },
};

export const WidgetCard = memo(function WidgetCard({
  widget,
  editMode,
  draggable = editMode,
  onDelete,
  dashboardParams,
  refreshKey,
  publicView,
  highlight,
}: Props) {
  // `widget.monitors` is present (even empty) whenever the widget is monitor-bound, so a public
  // widget whose monitors all fail the viewer gate stays a monitor card.
  const isMonitor = isMonitorWidget(widget);
  const monitorMeta = isMonitor ? computeMonitorCardMeta(widget) : null;
  const title = monitorMeta ? monitorMeta.title : widgetTitle(widget);

  const linkMonitors = !editMode && !publicView;
  const monitorHref =
    linkMonitors && monitorMeta?.monitorId != null ? `/monitors/${monitorMeta.monitorId}` : undefined;
  const chrome = CHROME[monitorMeta?.treatment ?? "neutral"];

  return (
    <div
      data-widget-id={widget.id}
      tabIndex={-1}
      className={`relative flex h-full flex-col rounded-md shadow-sm outline-none has-[a:focus-visible]:ring-2 has-[a:focus-visible]:ring-ring ${chrome.root}${draggable ? " select-none" : ""}${highlight ? " ring-2 ring-primary ring-offset-2 ring-offset-background" : ""} transition-shadow duration-300 motion-reduce:transition-none`}
    >
      <div
        className={`flex flex-wrap items-start justify-between gap-x-2 gap-y-1 px-3 py-2 ${chrome.header}${draggable ? " tile-drag-handle cursor-grab active:cursor-grabbing select-none" : ""}`}
      >
        {draggable ? (
          <div data-testid="widget-title-group" className="flex min-w-[min(8rem,100%)] flex-1 items-start gap-1.5">
            <GripVertical aria-hidden className="mt-0.5 h-3.5 w-3.5 shrink-0 text-muted-foreground" />
            <TruncatedTitle title={title} href={monitorHref} minW="min-w-0" />
          </div>
        ) : (
          <TruncatedTitle title={title} href={monitorHref} />
        )}
        {editMode && (
          <div className="ml-auto flex shrink-0 items-center gap-1">
            <Button
              variant="ghost"
              size="icon"
              className="h-6 w-6 text-destructive hover:text-destructive"
              aria-label={`Remove ${title}`}
              title={`Remove ${title}`}
              onClick={onDelete}
            >
              <X className="h-3.5 w-3.5" />
            </Button>
          </div>
        )}
        {!editMode &&
          widget.visualization_id != null &&
          widget.freshness &&
          (!publicView || widget.freshness.expected_interval_secs != null) && (
          <TileFreshnessDot freshness={widget.freshness} alwaysShowAge={publicView ?? false} />
        )}
      </div>
      <div className="min-h-0 flex-1 overflow-hidden p-2">
        {widget.visualization_id != null ? (
          <VizWidget widget={widget} dashboardParams={dashboardParams} refreshKey={refreshKey} />
        ) : isMonitor ? (
          <MonitorWidget widget={widget} linkable={linkMonitors} />
        ) : (
          <TextWidget text={widget.text} />
        )}
      </div>
    </div>
  );
});
