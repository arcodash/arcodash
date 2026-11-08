import { useEffect, useMemo, useState } from "react";
import { useResizableColumn } from "@/hooks/useResizableColumn";
import { ColumnResizeHandle } from "@/components/common/ColumnResizeHandle";
import { keepPreviousData, useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Link, useNavigate } from "react-router";
import { AlertTriangle, CircleStop, Clock, Edit, Lock, Play, Plus, Star, Terminal, Trash2 } from "lucide-react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Loader, PageLoader } from "@/components/ui/loader";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { Card, CardContent, CardFooter, CardHeader } from "@/components/ui/card";
import { ConfirmDialog } from "@/components/ui/confirm-dialog";
import { PageHeader } from "@/components/common/PageHeader";
import { EmptyState } from "@/components/common/EmptyState";
import { UnpublishedBadge } from "@/components/common/StateBadge";
import { ListToolbar, ownerChipGroup, TagFilterPills, type ChipGroupFilter } from "@/components/common/ListToolbar";
import { ExportMenu } from "@/components/query-editor/ExportMenu";
import { ExecutionStatusIndicator } from "@/components/query-editor/ExecutionStatusIndicator";
import { OwnerChip } from "@/components/sharing/OwnerChip";
import { VisibilityChip } from "@/components/sharing/VisibilityChip";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { usePollJob } from "@/lib/jobs";
import { useDebouncedValue } from "@/lib/useDebouncedValue";
import { useListView } from "@/lib/useListView";
import { useAuth } from "@/lib/auth";
import { usePermissions } from "@/lib/permissions";
import { ApiError, api, apiErrorMessage } from "@/lib/api";
import { effectiveState } from "@/lib/effectiveState";
import { cn } from "@/lib/utils";
import { describeSpec, exactTs, impactLine, isSchedulePaused } from "@/lib/scheduling";
import type { Query, Schedule, ScopeFilter } from "@/types/api";

/** Run/Stop behavior for one query row, shared by table and card renderers so a view switch cannot
 * change it. */
function useQueryRunActions(q: Query) {
  const queryClient = useQueryClient();
  const [activeJobId, setActiveJobId] = useState<string | null>(null);
  const [runError, setRunError] = useState<string | null>(null);
  const job = usePollJob(activeJobId);
  const canRun = q.caller.can_execute ?? false;

  const cancelMutation = useMutation({
    mutationFn: () => {
      if (activeJobId == null) throw new Error("No job to stop");
      return api.cancelJob(activeJobId);
    },
    onError: (err) => {
      // 404: the job already reached a terminal state; the poll settles on its own.
      if (err instanceof ApiError && err.status === 404) return;
      setRunError(apiErrorMessage(err, "Failed to stop the query"));
    },
  });

  useEffect(() => {
    if (!activeJobId || !job.isDone) return;
    // Cancelled carries `error: null` and must not hit the failure copy; the icon reverting to Play
    // is confirmation enough.
    if (job.isFailure) {
      setRunError(job.error ?? "Query run failed");
    } else if (job.isCancelled) {
      setRunError(null);
    } else if (job.gaveUp) {
      setRunError(job.error);
    } else {
      setRunError(null);
      queryClient.invalidateQueries({ queryKey: ["queries"] });
    }
    setActiveJobId(null);
    // Else a stale `isSuccess` from this cycle's cancel pins the button on "Stopping…".
    cancelMutation.reset();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [activeJobId, job.isDone, job.isFailure, job.isCancelled, job.gaveUp, job.error, queryClient]);

  const refreshMutation = useMutation({
    mutationFn: () => api.refreshQuery(q.id),
    onSuccess: (res) => {
      setRunError(null);
      if (res.query_result) {
        queryClient.invalidateQueries({ queryKey: ["queries"] });
      } else if (res.job) {
        setActiveJobId(res.job.id);
      }
    },
    onError: (err) => setRunError(apiErrorMessage(err, "Failed to run query")),
  });

  const favMutation = useMutation({
    mutationFn: () => (q.is_favorite ? api.unstarQuery(q.id) : api.starQuery(q.id)),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ["queries"] }),
    onError: (err) => setRunError(apiErrorMessage(err, "Failed to update favorite")),
  });

  const canStop = activeJobId !== null && !job.isDone;
  const isStopping = cancelMutation.isPending || cancelMutation.isSuccess;
  const runLabel = isStopping ? "Stopping…" : canStop ? "Stop run" : refreshMutation.isPending ? "Running…" : "Run";

  return {
    canRun,
    canStop,
    isStopping,
    isRunning: refreshMutation.isPending,
    runLabel,
    runError,
    onRunClick: () => (canStop ? cancelMutation.mutate() : refreshMutation.mutate()),
    toggleFavorite: () => favMutation.mutate(),
    isFavoritePending: favMutation.isPending,
  };
}

function QueryActions({
  query: q,
  run,
  onDeleteClick,
}: {
  query: Query;
  run: ReturnType<typeof useQueryRunActions>;
  onDeleteClick: (q: Query) => void;
}) {
  const navigate = useNavigate();
  const canEdit = q.caller.can_edit;

  return (
    <div
      className="flex items-center gap-1"
      onClick={(e) => e.stopPropagation()}
      onKeyDown={(e) => e.stopPropagation()}
    >
      <Button
        variant="ghost"
        size="icon-sm"
        className="h-7 w-7"
        title={q.is_favorite ? "Remove from favorites" : "Add to favorites"}
        aria-label={q.is_favorite ? "Remove from favorites" : "Add to favorites"}
        disabled={run.isFavoritePending}
        onClick={run.toggleFavorite}
      >
        <Star className={`w-3.5 h-3.5 ${q.is_favorite ? "fill-yellow-400 text-yellow-400" : ""}`} />
      </Button>
      {run.canRun ? (
        <Button
          variant="ghost"
          size="icon-sm"
          className="h-7 w-7"
          title={run.runLabel}
          aria-label={run.runLabel}
          disabled={run.isStopping || run.isRunning}
          onClick={run.onRunClick}
        >
          {run.isStopping || run.isRunning ? (
            <Loader size={16} />
          ) : run.canStop ? (
            <CircleStop className="w-3.5 h-3.5" />
          ) : (
            <Play className="w-3.5 h-3.5" />
          )}
        </Button>
      ) : (
        <span
          className="inline-flex h-7 w-7 items-center justify-center text-muted-foreground"
          title="Limited access — some actions unavailable. Ask an admin."
          aria-label="Limited access — some actions unavailable. Ask an admin."
        >
          <Lock className="w-3.5 h-3.5" />
        </span>
      )}
      <ExportMenu resultId={q.latest_query_data_id} />
      {canEdit && (
        <>
          <Button
            variant="ghost"
            size="icon-sm"
            className="h-7 w-7"
            title="Edit"
            onClick={() => navigate(`/queries/${q.id}`)}
          >
            <Edit className="w-3.5 h-3.5" />
          </Button>
          <Button
            variant="ghost"
            size="icon-sm"
            className="h-7 w-7 text-destructive hover:text-destructive"
            title="Delete"
            onClick={() => onDeleteClick(q)}
          >
            <Trash2 className="w-3.5 h-3.5" />
          </Button>
        </>
      )}
    </div>
  );
}

// Status/row count are shared columns updated by draft or published runs, but this list exposes only
// the draft's result pointer, so a published-only run can show Success while `latest_query_data_id`
// is null. Export keys off that pointer; this stops the status cells claiming a result Export cannot
// reach.
const isResultUnreachable = (q: Query) => q.last_execution_status === "Success" && q.latest_query_data_id == null;

function QueryTags({ tags }: { tags: string[] | undefined }) {
  if (!tags?.length) return null;
  return (
    <div className="flex flex-wrap gap-1">
      {tags.map((t) => (
        <span key={t} className="rounded-full bg-muted px-2 py-0.5 text-xs text-muted-foreground">
          {t}
        </span>
      ))}
    </div>
  );
}

function QueryStateBadges({ query: q, className }: { query: Query; className?: string }) {
  const state = effectiveState(q, q.caller.can_edit ? "editor" : "viewer");
  return (
    <>
      {q.is_archived && (
        <Badge variant="outline" className={cn(className, "text-muted-foreground")}>
          Archived
        </Badge>
      )}
      {state.pending && <UnpublishedBadge className={className} />}
    </>
  );
}

function scheduleLabel(schedule: Schedule | null): string {
  if (!schedule) return "—";
  if (!schedule.enabled) return "Off";
  return isSchedulePaused(schedule) ? "Paused" : describeSpec(schedule.spec);
}

interface RowProps {
  query: Query;
  dataSourceName: string;
  ownerName: string;
  isOwnerMe: boolean;
  schedule: Schedule | null;
  onDeleteClick: (q: Query) => void;
}

function QueryTableRow({ query: q, dataSourceName, ownerName, isOwnerMe, schedule, onDeleteClick }: RowProps) {
  const navigate = useNavigate();
  const run = useQueryRunActions(q);
  const resultUnreachable = isResultUnreachable(q);

  return (
    // No `role="button"`/`tabIndex`: that would override the row's native ARIA "row" role.
    // Keyboard, right-click and new-tab go through the `<Link>` in the name cell; `onClick` is a
    // pointer-only convenience.
    <TableRow onClick={() => navigate(`/queries/${q.id}`)} className="cursor-pointer border-t border-border hover:bg-muted/30">
      <TableCell className="w-full max-w-0">
        <div className="flex min-w-0 flex-col gap-1">
          <div className="flex min-w-0 flex-wrap items-center gap-2">
            <div className="min-w-0 flex-1">
              <Tooltip>
                <TooltipTrigger asChild>
                  <Link
                    to={`/queries/${q.id}`}
                    className="block truncate text-sm font-medium hover:underline"
                    onClick={(e) => e.stopPropagation()}
                  >
                    {q.name || "Untitled query"}
                  </Link>
                </TooltipTrigger>
                <TooltipContent>{q.name || "Untitled query"}</TooltipContent>
              </Tooltip>
              {q.description && (
                <Tooltip>
                  <TooltipTrigger asChild>
                    <span className="block truncate text-xs text-muted-foreground">
                      {q.description}
                    </span>
                  </TooltipTrigger>
                  <TooltipContent>{q.description}</TooltipContent>
                </Tooltip>
              )}
              {run.runError && <div className="text-xs text-destructive">{run.runError}</div>}
            </div>
            <span className="shrink-0">
              <VisibilityChip visibility={q.visibility} isDraft={!q.published} objectType="query" />
            </span>
            <QueryStateBadges query={q} className="shrink-0" />
          </div>
          <QueryTags tags={q.tags} />
        </div>
      </TableCell>
      <TableCell className="hidden @min-[54rem]/list:table-cell w-px whitespace-nowrap text-muted-foreground">
        <span className="block max-w-48 truncate" title={dataSourceName}>
          {dataSourceName}
        </span>
      </TableCell>
      <TableCell className="hidden @min-[71rem]/list:table-cell w-px whitespace-nowrap text-muted-foreground">
        <OwnerChip name={ownerName} isCurrentUser={isOwnerMe} showName />
      </TableCell>
      <TableCell className="w-px whitespace-nowrap">
        {resultUnreachable ? (
          <Tooltip>
            <TooltipTrigger asChild>
              <span className="flex items-center gap-1.5 text-sm text-muted-foreground">
                <AlertTriangle className="w-4 h-4 text-warning" /> Result unavailable
              </span>
            </TooltipTrigger>
            <TooltipContent>
              This query last ran successfully, but its result can no longer be reached — run it
              again to refresh.
            </TooltipContent>
          </Tooltip>
        ) : (
          <ExecutionStatusIndicator status={q.last_execution_status} />
        )}
      </TableCell>
      <TableCell className="hidden @min-[68rem]/list:table-cell w-px whitespace-nowrap text-muted-foreground">
        {q.last_execution_at ? exactTs(Date.parse(q.last_execution_at)) : "Never"}
      </TableCell>
      <TableCell className="hidden @min-[44rem]/list:table-cell w-px whitespace-nowrap text-right text-muted-foreground">
        {resultUnreachable ? "—" : (q.row_count ?? "—")}
      </TableCell>
      <TableCell className="hidden @min-[64rem]/list:table-cell w-px whitespace-nowrap">
        {schedule?.enabled ? (
          <span className="flex items-center gap-1 text-xs text-muted-foreground">
            <Clock className="w-3.5 h-3.5" />
            {scheduleLabel(schedule)}
          </span>
        ) : (
          <span className="text-xs text-muted-foreground">{scheduleLabel(schedule)}</span>
        )}
      </TableCell>
      <TableCell className="w-px whitespace-nowrap">
        <QueryActions query={q} run={run} onDeleteClick={onDeleteClick} />
      </TableCell>
    </TableRow>
  );
}

function QueryCard({ query: q, dataSourceName, ownerName, isOwnerMe, schedule, onDeleteClick }: RowProps) {
  const navigate = useNavigate();
  const run = useQueryRunActions(q);
  const resultUnreachable = isResultUnreachable(q);

  return (
    <Card
      className="cursor-pointer transition-colors hover:bg-muted/50"
      onClick={() => navigate(`/queries/${q.id}`)}
    >
      <CardHeader className="pb-3">
        <Link
          to={`/queries/${q.id}`}
          className="block truncate text-sm font-medium hover:underline"
          onClick={(e) => e.stopPropagation()}
        >
          {q.name || "Untitled query"}
        </Link>
        <div className="flex flex-wrap items-center gap-2">
          <VisibilityChip visibility={q.visibility} isDraft={!q.published} objectType="query" />
          <QueryStateBadges query={q} />
        </div>
        <div className="flex items-center gap-2 text-xs text-muted-foreground">
          <OwnerChip name={ownerName} isCurrentUser={isOwnerMe} showName />
          <span className="truncate">{dataSourceName}</span>
        </div>
        <QueryTags tags={q.tags} />
      </CardHeader>
      <CardContent className="space-y-1.5 text-xs text-muted-foreground">
        <div className="flex items-center justify-between gap-2">
          {resultUnreachable ? (
            <span className="flex items-center gap-1.5">
              <AlertTriangle className="w-3.5 h-3.5 text-warning" /> Result unavailable
            </span>
          ) : (
            <ExecutionStatusIndicator status={q.last_execution_status} />
          )}
          <span>{resultUnreachable ? "—" : (q.row_count ?? "—")} rows</span>
        </div>
        <div className="flex items-center justify-between gap-2">
          <span>{q.last_execution_at ? exactTs(Date.parse(q.last_execution_at)) : "Never run"}</span>
          <span className="flex items-center gap-1">
            <Clock className="w-3 h-3" />
            {scheduleLabel(schedule)}
          </span>
        </div>
        {run.runError && <div className="text-destructive">{run.runError}</div>}
      </CardContent>
      <CardFooter className="border-t border-border pt-3">
        <QueryActions query={q} run={run} onDeleteClick={onDeleteClick} />
      </CardFooter>
    </Card>
  );
}

export function QueriesPage() {
  const nameCol = useResizableColumn("arcodash.listcol.queries.name");
  const { user: currentUser } = useAuth();
  const { has } = usePermissions();
  const canCreate = has("create_query");
  const [search, setSearch] = useState("");
  const debouncedSearch = useDebouncedValue(search, 250);
  const [pendingDelete, setPendingDelete] = useState<Query | null>(null);
  const [deleteError, setDeleteError] = useState<string | null>(null);
  const [scope, setScope] = useState<ScopeFilter>("all");
  const [status, setStatus] = useState<"all" | "draft" | "published">("all");
  const [showArchived, setShowArchived] = useState(false);
  const [favOnly, setFavOnly] = useState(false);
  const [activeTag, setActiveTag] = useState<string | null>(null);
  const queryClient = useQueryClient();
  const { view, setView, isMobile } = useListView("list-view:queries");

  const queriesQuery = useQuery({
    queryKey: ["queries", debouncedSearch, scope, showArchived],
    queryFn: () =>
      api.listQueries(showArchived, debouncedSearch || undefined, scope === "all" ? undefined : scope),
    placeholderData: keepPreviousData,
  });

  const tagsQ = useQuery({
    queryKey: ["query-tags"],
    queryFn: () => api.getQueryTags(),
  });
  const allTags = tagsQ.data?.tags ?? [];

  const dataSourcesQuery = useQuery({
    queryKey: ["data-sources"],
    queryFn: () => api.listDataSources(),
  });

  const schedulesQuery = useQuery({
    queryKey: ["schedules", "all"],
    queryFn: () => api.listSchedules(),
  });

  const dsName = (id: number) =>
    dataSourcesQuery.data?.data_sources.find((d) => d.id === id)?.name ?? "—";

  const scheduleByQueryId = useMemo(
    () =>
      new Map((schedulesQuery.data ?? []).filter((s) => s.entity_type === "query").map((s) => [s.entity_id, s])),
    [schedulesQuery.data],
  );

  const rows = (queriesQuery.data ?? []).filter(
    (q) =>
      (status === "all" || !q.published === (status === "draft")) &&
      (!favOnly || q.is_favorite) &&
      (!activeTag || (q.tags ?? []).includes(activeTag)),
  );
  const filtersActive =
    search !== "" || status !== "all" || scope !== "all" || showArchived || favOnly || activeTag !== null;

  const deleteMutation = useMutation({
    mutationFn: (id: number) => api.deleteQuery(id),
    onSuccess: (_data, id) => {
      queryClient.invalidateQueries({ queryKey: ["queries"] });
      queryClient.invalidateQueries({ queryKey: ["query-tags"] });
      queryClient.invalidateQueries({ queryKey: ["dashboards"] });
      queryClient.invalidateQueries({ queryKey: ["monitors"] });
      queryClient.invalidateQueries({ queryKey: ["schedules"] });
      queryClient.removeQueries({ queryKey: ["query", id] });
      setPendingDelete(null);
    },
    onError: (err) => setDeleteError(apiErrorMessage(err, "Failed to delete")),
  });

  const consumersQuery = useQuery({
    queryKey: ["consumers", pendingDelete?.id],
    queryFn: () => api.getQueryConsumers(pendingDelete!.id),
    enabled: pendingDelete != null,
  });
  const deleteImpact = consumersQuery.data ? impactLine(consumersQuery.data) : null;

  const chipGroups: ChipGroupFilter[] = [
    ownerChipGroup(scope, setScope),
    {
      key: "status",
      groupLabel: "State",
      value: status,
      onChange: (v) => setStatus(v as typeof status),
      options: [
        { value: "all", label: "All states" },
        { value: "draft", label: "Draft" },
        { value: "published", label: "Published" },
      ],
    },
  ];
  const activeFilterCount =
    (scope !== "all" ? 1 : 0) + (status !== "all" ? 1 : 0) + (showArchived ? 1 : 0) + (favOnly ? 1 : 0);

  function onDeleteClick(q: Query) {
    setDeleteError(null);
    setPendingDelete(q);
  }

  return (
    <div className="space-y-6">
      <PageHeader
        title="Queries"
        action={
          canCreate ? (
            <Button size="sm" asChild>
              <Link to="/queries/new">
                <Plus className="w-3 h-3" />
                New Query
              </Link>
            </Button>
          ) : undefined
        }
      />

      <ListToolbar
        search={search}
        onSearchChange={setSearch}
        searchPlaceholder="Search queries…"
        chipGroups={chipGroups}
        toggles={[
          {
            key: "favorites",
            label: "Favorites",
            icon: Star,
            active: favOnly,
            onToggle: () => setFavOnly((f) => !f),
          },
          {
            key: "archived",
            label: "Show archived",
            active: showArchived,
            onToggle: () => setShowArchived((v) => !v),
          },
        ]}
        activeFilterCount={activeFilterCount}
        isFetching={queriesQuery.isFetching && !queriesQuery.isLoading}
        view={view}
        onViewChange={setView}
        showViewToggle={!isMobile}
      />

      <TagFilterPills tags={allTags} active={activeTag} onChange={setActiveTag} />

      {queriesQuery.isLoading && <PageLoader />}

      {queriesQuery.isError && (
        <p className="text-destructive">
          {apiErrorMessage(queriesQuery.error, "Failed to load queries")}
        </p>
      )}

      {queriesQuery.data && rows.length === 0 && filtersActive && (
        <EmptyState
          icon={Terminal}
          iconClassName="bg-indigo-50 text-indigo-600"
          title="No queries match these filters"
          description="Try adjusting or clearing your search and filters."
          action={
            <Button
              size="sm"
              variant="outline"
              onClick={() => {
                setSearch("");
                setStatus("all");
                setScope("all");
                setShowArchived(false);
                setFavOnly(false);
                setActiveTag(null);
              }}
            >
              Clear filters
            </Button>
          }
        />
      )}

      {queriesQuery.data && rows.length === 0 && !filtersActive && (
        <EmptyState
          icon={Terminal}
          iconClassName="bg-indigo-50 text-indigo-600"
          title="Write your first query"
          description="Run SQL against any connected data source and reuse the results in dashboards and monitors."
          action={
            canCreate && (
              <Button size="sm" asChild>
                <Link to="/queries/new">
                  <Plus className="w-3 h-3" />
                  Create Your First Query
                </Link>
              </Button>
            )
          }
        />
      )}

      {queriesQuery.data && rows.length > 0 && view === "table" && (
        <div className="@container/list">
          <Table className="w-full">
            <TableHeader className="bg-muted/50">
              <TableRow>
                <TableHead ref={nameCol.headRef} style={{ minWidth: nameCol.width ?? undefined }} className="relative w-full max-w-0 px-4 py-3 text-xs font-medium">
                  Name
                  <ColumnResizeHandle column={nameCol} />
                </TableHead>
                <TableHead className="hidden @min-[54rem]/list:table-cell w-px whitespace-nowrap px-4 py-3 text-xs font-medium">
                  Data Source
                </TableHead>
                <TableHead className="hidden @min-[71rem]/list:table-cell w-px whitespace-nowrap px-4 py-3 text-xs font-medium">
                  Owner
                </TableHead>
                <TableHead className="w-px whitespace-nowrap px-4 py-3 text-xs font-medium">Status</TableHead>
                <TableHead className="hidden @min-[68rem]/list:table-cell w-px whitespace-nowrap px-4 py-3 text-xs font-medium">
                  Last Executed
                </TableHead>
                <TableHead className="hidden @min-[44rem]/list:table-cell w-px whitespace-nowrap px-4 py-3 text-right text-xs font-medium">
                  Rows
                </TableHead>
                <TableHead className="hidden @min-[64rem]/list:table-cell w-px whitespace-nowrap px-4 py-3 text-xs font-medium">
                  Schedule
                </TableHead>
                <TableHead className="w-px whitespace-nowrap px-4 py-3 text-xs font-medium">
                  <span className="sr-only">Actions</span>
                </TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {rows.map((q) => (
                <QueryTableRow
                  key={q.id}
                  query={q}
                  dataSourceName={dsName(q.data_source_id)}
                  ownerName={q.owner_name ?? "—"}
                  isOwnerMe={q.user_id === currentUser?.id}
                  schedule={scheduleByQueryId.get(q.id) ?? null}
                  onDeleteClick={onDeleteClick}
                />
              ))}
            </TableBody>
          </Table>
        </div>
      )}

      {queriesQuery.data && rows.length > 0 && view === "cards" && (
        <div className="grid grid-cols-1 gap-4 md:grid-cols-2 lg:grid-cols-3">
          {rows.map((q) => (
            <QueryCard
              key={q.id}
              query={q}
              dataSourceName={dsName(q.data_source_id)}
              ownerName={q.owner_name ?? "—"}
              isOwnerMe={q.user_id === currentUser?.id}
              schedule={scheduleByQueryId.get(q.id) ?? null}
              onDeleteClick={onDeleteClick}
            />
          ))}
        </div>
      )}

      <ConfirmDialog
        open={pendingDelete !== null}
        onOpenChange={(open) => !open && setPendingDelete(null)}
        title="Delete query?"
        description={`This will permanently delete "${pendingDelete?.name || "Untitled query"}".`}
        body={
          deleteImpact && (
            <p className="rounded-md border border-warning/30 bg-warning/10 px-3 py-2 text-sm text-warning-foreground">
              {deleteImpact}. Deleting this query removes them.
            </p>
          )
        }
        error={deleteError}
        destructive
        confirmLabel="Delete"
        pendingLabel="Deleting…"
        confirmPending={deleteMutation.isPending}
        allowCancelWhilePending
        onConfirm={() => pendingDelete && deleteMutation.mutate(pendingDelete.id)}
      />
    </div>
  );
}
