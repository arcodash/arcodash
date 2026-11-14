import { useState } from "react";
import { useResizableColumn } from "@/hooks/useResizableColumn";
import { ColumnResizeHandle } from "@/components/common/ColumnResizeHandle";
import { useMutation, useQueries, useQuery, useQueryClient } from "@tanstack/react-query";
import { Link, useNavigate, useSearchParams } from "react-router";
import { BellRing, Clock, Plus, Sparkles, Trash2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { Card, CardContent, CardFooter, CardHeader, CardTitle } from "@/components/ui/card";
import { ConfirmDialog } from "@/components/ui/confirm-dialog";
import { PageLoader } from "@/components/ui/loader";
import { PageHeader } from "@/components/common/PageHeader";
import { EmptyState } from "@/components/common/EmptyState";
import { ListToolbar, ownerChipGroup } from "@/components/common/ListToolbar";
import { OwnerChip } from "@/components/sharing/OwnerChip";
import { VisibilityChip } from "@/components/sharing/VisibilityChip";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { MonitorsTabs } from "@/components/monitors/MonitorsTabs";
import { StateBadge } from "@/components/common/StateBadge";
import { api, apiErrorMessage } from "@/lib/api";
import { useDebouncedValue } from "@/lib/useDebouncedValue";
import { useListView } from "@/lib/useListView";
import { usePermissions } from "@/lib/permissions";
import { useAuth } from "@/lib/auth";
import { AddMonitorWithAiDialog } from "@/components/monitors/AddMonitorWithAiDialog";
import { countdown, describeSpec, exactTs, isSchedulePaused } from "@/lib/scheduling";
import { matchesSearch } from "@/lib/utils";
import type { Monitor, Schedule, ScopeFilter } from "@/types/api";

function scheduleLabel(schedule: Schedule | null): string {
  if (!schedule) return "—";
  if (!schedule.enabled) return "Off";
  if (isSchedulePaused(schedule)) return "Paused";
  const base = describeSpec(schedule.spec);
  // A paused schedule's next_run_at is stale; never show a countdown for a run that won't happen.
  return schedule.next_run_at
    ? `${base} · next ${countdown(Date.parse(schedule.next_run_at), Date.now())}`
    : base;
}

/** Delete, identical in the table row and the card footer so switching view never changes what a
 * row can do. Edit is just opening the monitor, so no separate button. */
function MonitorActions({
  monitor: m,
  onDeleteClick,
}: {
  monitor: Monitor;
  onDeleteClick: (m: Monitor) => void;
}) {
  if (!m.caller.can_edit) return null;
  return (
    <div
      className="flex items-center gap-1"
      onClick={(e) => e.stopPropagation()}
      onKeyDown={(e) => e.stopPropagation()}
    >
      <Button
        variant="ghost"
        size="icon-sm"
        className="h-7 w-7 text-destructive hover:text-destructive"
        title="Delete"
        aria-label="Delete monitor"
        onClick={() => onDeleteClick(m)}
      >
        <Trash2 className="h-3.5 w-3.5" />
      </Button>
    </div>
  );
}

function NoSubscribersBadge() {
  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <Badge className="bg-warning/20 text-warning-foreground">No subscribers</Badge>
      </TooltipTrigger>
      <TooltipContent>
        No one is subscribed — this monitor won&apos;t notify anyone when it triggers.
      </TooltipContent>
    </Tooltip>
  );
}

interface RowProps {
  monitor: Monitor;
  schedule: Schedule | null;
  unsubscribed: boolean;
  onDeleteClick: (m: Monitor) => void;
}

function MonitorTableRow({ monitor: a, schedule, unsubscribed, onDeleteClick }: RowProps) {
  const navigate = useNavigate();
  return (
    <TableRow onClick={() => navigate(`/monitors/${a.id}`)} className="cursor-pointer border-t border-border hover:bg-muted/30">
      <TableCell className="w-full max-w-0">
        <div className="flex min-w-0 flex-wrap items-center gap-2">
          <div className="min-w-0 flex-1">
            <Tooltip>
              <TooltipTrigger asChild>
                <Link
                  to={`/monitors/${a.id}`}
                  className="block truncate text-primary underline"
                  onClick={(e) => e.stopPropagation()}
                >
                  {a.name}
                </Link>
              </TooltipTrigger>
              <TooltipContent>{a.name}</TooltipContent>
            </Tooltip>
          </div>
          <span className="shrink-0">
            <OwnerChip name={a.user.name} showName />
          </span>
          <span className="shrink-0">
            <VisibilityChip visibility={a.visibility} objectType="monitor" />
          </span>
        </div>
      </TableCell>
      <TableCell className="hidden @min-[54rem]/list:table-cell w-px whitespace-nowrap">
        <Link
          to={`/queries/${a.query.id}`}
          className="text-primary underline"
          onClick={(e) => e.stopPropagation()}
        >
          {a.query.name}
        </Link>
      </TableCell>
      <TableCell className="w-px whitespace-nowrap">
        <div className="flex items-center gap-1.5">
          <StateBadge state={a.state} />
          {unsubscribed && <NoSubscribersBadge />}
        </div>
      </TableCell>
      <TableCell className="hidden @min-[64rem]/list:table-cell w-px whitespace-nowrap text-muted-foreground text-sm">
        {schedule?.enabled ? (
          <span className="flex items-center gap-1">
            <Clock className="w-3.5 h-3.5" />
            {scheduleLabel(schedule)}
          </span>
        ) : (
          scheduleLabel(schedule)
        )}
      </TableCell>
      <TableCell className="hidden @min-[71rem]/list:table-cell w-px whitespace-nowrap text-muted-foreground text-sm">
        {a.last_triggered_at ? exactTs(Date.parse(a.last_triggered_at)) : "—"}
      </TableCell>
      <TableCell className="w-px whitespace-nowrap">
        <MonitorActions monitor={a} onDeleteClick={onDeleteClick} />
      </TableCell>
    </TableRow>
  );
}

function MonitorCard({ monitor: a, schedule, unsubscribed, onDeleteClick }: RowProps) {
  const navigate = useNavigate();
  return (
    <Card
      className="cursor-pointer transition-colors hover:bg-muted/50"
      onClick={() => navigate(`/monitors/${a.id}`)}
    >
      <CardHeader className="pb-3">
        <div className="flex items-start justify-between gap-2">
          <Link to={`/monitors/${a.id}`} onClick={(e) => e.stopPropagation()}>
            <CardTitle className="text-base hover:underline">{a.name}</CardTitle>
          </Link>
          <div className="flex items-center gap-1.5">
            <StateBadge state={a.state} />
            {unsubscribed && <NoSubscribersBadge />}
          </div>
        </div>
        <div className="flex flex-wrap items-center gap-2">
          <OwnerChip name={a.user.name} showName />
          <VisibilityChip visibility={a.visibility} objectType="monitor" />
        </div>
        <Link
          to={`/queries/${a.query.id}`}
          className="truncate text-xs text-primary underline"
          onClick={(e) => e.stopPropagation()}
        >
          {a.query.name}
        </Link>
      </CardHeader>
      <CardContent className="space-y-1 text-xs text-muted-foreground">
        <div className="flex items-center gap-1">
          <Clock className="w-3 h-3" />
          {scheduleLabel(schedule)}
        </div>
        <div>Last triggered: {a.last_triggered_at ? exactTs(Date.parse(a.last_triggered_at)) : "—"}</div>
      </CardContent>
      <CardFooter className="border-t border-border pt-3">
        <MonitorActions monitor={a} onDeleteClick={onDeleteClick} />
      </CardFooter>
    </Card>
  );
}

export function MonitorsPage() {
  const nameCol = useResizableColumn("arcodash.listcol.monitors.name");
  const navigate = useNavigate();
  const [searchParams, setSearchParams] = useSearchParams();
  const queryIdParam = Number(searchParams.get("query_id"));
  const queryId = Number.isInteger(queryIdParam) && queryIdParam > 0 ? queryIdParam : undefined;
  const qc = useQueryClient();
  const { has } = usePermissions();
  const canCreate = has("create_monitor");
  const { clientConfig } = useAuth();
  const canAskAi =
    canCreate &&
    has("create_query") &&
    !!clientConfig?.ai_sql_generation &&
    !!clientConfig?.ai_monitor_suggestions;
  const [askOpen, setAskOpen] = useState(false);
  const [scope, setScope] = useState<ScopeFilter>("all");
  const [search, setSearch] = useState("");
  const debouncedSearch = useDebouncedValue(search, 250);
  const [pendingDelete, setPendingDelete] = useState<Monitor | null>(null);
  const [deleteError, setDeleteError] = useState<string | null>(null);
  const { view, setView, isMobile } = useListView("list-view:monitors");

  const monitorsQuery = useQuery({
    queryKey: ["monitors", scope, queryId],
    queryFn: () => api.listMonitors(scope === "all" ? undefined : scope, queryId),
  });
  // Same queryKey shape as the monitor detail page's subscriptions fetch, so they share one cache
  // entry.
  const subsQueries = useQueries({
    queries: (monitorsQuery.data ?? []).map((m) => ({
      queryKey: ["monitor-subs", String(m.id)],
      queryFn: () => api.listMonitorSubscriptions(m.id),
    })),
  });
  const unsubscribedIds = new Set(
    (monitorsQuery.data ?? [])
      .filter((_, i) => subsQueries[i]?.data?.length === 0)
      .map((m) => m.id),
  );
  // Reuses the ["schedules","all"] key QueriesPage already caches.
  const schedulesQuery = useQuery({
    queryKey: ["schedules", "all"],
    queryFn: () => api.listSchedules(),
  });
  const scheduleFor = (monitorId: number) =>
    schedulesQuery.data?.find((s) => s.entity_type === "monitor" && s.entity_id === monitorId) ?? null;

  // No `q=` param on GET /api/monitors; the full list is fetched, so search filters in memory.
  const filtered = (monitorsQuery.data ?? []).filter((m) => matchesSearch(debouncedSearch, m.name, m.query.name));
  const filtersActive = search !== "" || scope !== "all" || queryId != null;

  const deleteMutation = useMutation({
    mutationFn: (id: number) => api.deleteMonitor(id),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["monitors"] });
      setPendingDelete(null);
    },
    onError: (err) => setDeleteError(apiErrorMessage(err, "Failed to delete monitor")),
  });

  const chipGroups = [ownerChipGroup(scope, setScope)];

  function onDeleteClick(m: Monitor) {
    setDeleteError(null);
    setPendingDelete(m);
  }

  return (
    <div className="mx-auto max-w-6xl space-y-6">
      <PageHeader
        title="Monitors"
        action={
          canCreate ? (
            <div className="flex flex-wrap items-center gap-2">
              {canAskAi && (
                <Button variant="outline" data-testid="monitor-ask-open" onClick={() => setAskOpen(true)}>
                  <Sparkles />
                  Add with AI
                </Button>
              )}
              <Button onClick={() => navigate("/monitors/new")}>+ New Monitor</Button>
            </div>
          ) : undefined
        }
      />
      {canAskAi && <AddMonitorWithAiDialog open={askOpen} onOpenChange={setAskOpen} />}
      <MonitorsTabs />

      <ListToolbar
        search={search}
        onSearchChange={setSearch}
        searchPlaceholder="Search monitors…"
        chipGroups={chipGroups}
        activeFilterCount={scope !== "all" ? 1 : 0}
        isFetching={monitorsQuery.isFetching && !monitorsQuery.isLoading}
        view={view}
        onViewChange={setView}
        showViewToggle={!isMobile}
      />

      {queryId != null && (
        <button
          type="button"
          className="text-sm text-muted-foreground underline-offset-2 hover:underline"
          onClick={() =>
            setSearchParams(
              (prev) => {
                const next = new URLSearchParams(prev);
                next.delete("query_id");
                return next;
              },
              { replace: true },
            )
          }
        >
          Filtered to one query · clear
        </button>
      )}

      {monitorsQuery.isLoading && <PageLoader />}
      {monitorsQuery.isError && (
        <p className="text-destructive">{apiErrorMessage(monitorsQuery.error, "Failed to load monitors")}</p>
      )}
      {monitorsQuery.data && filtered.length === 0 && filtersActive && (
        <EmptyState
          icon={BellRing}
          iconClassName="bg-warning/10 text-warning"
          title="No monitors match these filters"
          description="Try adjusting or clearing your search and filters."
          action={
            <Button
              size="sm"
              variant="outline"
              onClick={() => {
                setSearch("");
                setScope("all");
              }}
            >
              Clear filters
            </Button>
          }
        />
      )}
      {monitorsQuery.data && monitorsQuery.data.length === 0 && !filtersActive && (
        <>
          <EmptyState
            icon={BellRing}
            iconClassName="bg-warning/10 text-warning"
            title="Set up your first monitor"
            description="Watch a query for data-observability issues and get alerted the moment something looks off."
            action={
              canCreate ? (
                <Button size="sm" onClick={() => navigate("/monitors/new")}>
                  <Plus className="w-3 h-3" />
                  Create Your First Monitor
                </Button>
              ) : undefined
            }
          />
          <p className="text-center text-sm text-muted-foreground">
            Not sure what to watch?{" "}
            {canAskAi ? (
              <>
                <button type="button" className="text-primary underline" onClick={() => setAskOpen(true)}>
                  Describe it to AI
                </button>{" "}
                or{" "}
                <Link to="/data_sources" className="text-primary underline">
                  let AI suggest checks from a data source's schema
                </Link>
                .
              </>
            ) : (
              <Link to="/data_sources" className="text-primary underline">
                Let AI suggest checks from a data source's schema.
              </Link>
            )}
          </p>
        </>
      )}
      {filtered.length > 0 && view === "table" && (
        <div className="@container/list">
          <Table className="w-full">
            <TableHeader>
              <TableRow>
                <TableHead ref={nameCol.headRef} style={{ minWidth: nameCol.width ?? undefined }} className="relative w-full max-w-0">
                  Name
                  <ColumnResizeHandle column={nameCol} />
                </TableHead>
                <TableHead className="hidden @min-[54rem]/list:table-cell w-px whitespace-nowrap">Query</TableHead>
                <TableHead className="w-px whitespace-nowrap">State</TableHead>
                <TableHead className="hidden @min-[64rem]/list:table-cell w-px whitespace-nowrap">
                  Check schedule
                </TableHead>
                <TableHead className="hidden @min-[71rem]/list:table-cell w-px whitespace-nowrap">
                  Last Triggered
                </TableHead>
                <TableHead className="w-px whitespace-nowrap">
                  <span className="sr-only">Actions</span>
                </TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {filtered.map((a) => (
                <MonitorTableRow
                  key={a.id}
                  monitor={a}
                  schedule={scheduleFor(a.id)}
                  unsubscribed={unsubscribedIds.has(a.id)}
                  onDeleteClick={onDeleteClick}
                />
              ))}
            </TableBody>
          </Table>
        </div>
      )}
      {filtered.length > 0 && view === "cards" && (
        <div className="grid grid-cols-1 gap-4 md:grid-cols-2 lg:grid-cols-3">
          {filtered.map((a) => (
            <MonitorCard
              key={a.id}
              monitor={a}
              schedule={scheduleFor(a.id)}
              unsubscribed={unsubscribedIds.has(a.id)}
              onDeleteClick={onDeleteClick}
            />
          ))}
        </div>
      )}

      <ConfirmDialog
        open={pendingDelete !== null}
        onOpenChange={(open) => {
          if (!open) {
            setPendingDelete(null);
            setDeleteError(null);
          }
        }}
        title="Delete this monitor?"
        description={`This will permanently delete "${pendingDelete?.name ?? ""}".`}
        confirmLabel="Delete"
        destructive
        confirmPending={deleteMutation.isPending}
        onConfirm={() => pendingDelete && deleteMutation.mutate(pendingDelete.id)}
        error={deleteError}
      />
    </div>
  );
}
