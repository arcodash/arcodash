import { useEffect, useRef, useState } from "react";
import { useTransientMessage } from "@/hooks/useTransientMessage";
import { useParams } from "react-router";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Badge } from "@/components/ui/badge";
import { Check, Eye, LayoutDashboard, Link2, Maximize2, Minimize2, Pencil, Plus, RefreshCw, Share2, Sparkles, Star } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Dialog, DialogContent, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { DropdownMenuItem, DropdownMenuLabel } from "@/components/ui/dropdown-menu";
import { ActionBar, type ActionItem } from "@/components/common/ActionBar";
import { EmptyState } from "@/components/common/EmptyState";
import { TagEditor } from "@/components/common/TagEditor";
import { UnpublishedBadge } from "@/components/common/StateBadge";
import { BackButton } from "@/components/nav/BackButton";
import { cn } from "@/lib/utils";
import { api, apiErrorMessage } from "@/lib/api";
import { effectiveState, publishDialogCopy } from "@/lib/effectiveState";
import { DashboardGrid } from "@/components/dashboard/DashboardGrid";
import { DashboardParamsBar } from "@/components/dashboard/DashboardParamsBar";
import { AddWidgetDialog } from "@/components/dashboard/AddWidgetDialog";
import { AddWithAiBar, type WidgetCreated } from "@/components/dashboard/AddWithAiBar";
import { ShareEmbedDialog } from "@/components/share/ShareEmbedDialog";
import { ShareDialog } from "@/components/sharing/ShareDialog";
import { OwnerChip } from "@/components/sharing/OwnerChip";
import { PageLoader } from "@/components/ui/loader";
import { ConfirmDialog } from "@/components/ui/confirm-dialog";
import { DashboardNarrativePanel, type NarrativeError } from "@/components/dashboard/DashboardNarrativePanel";
import { aiErrorMessage } from "@/lib/ai";
import { useAuth } from "@/lib/auth";
import { usePermissions } from "@/lib/permissions";
import { useMediaQuery } from "@/lib/useMediaQuery";
import { useDocumentTitle } from "@/lib/useDocumentTitle";
import type { ChangeSummary, DashboardNarrative, ParamValue } from "@/types/api";

/** "3 tiles added · 1 removed · 2 moved · 1 resized", zero-count clauses omitted; option-only
 * changes read "Tile settings updated". */
function changeSummaryLine(s: ChangeSummary): string {
  const parts: string[] = [];
  if (s.added > 0) parts.push(`${s.added} tile${s.added === 1 ? "" : "s"} added`);
  if (s.removed > 0) parts.push(`${s.removed} removed`);
  if (s.moved > 0) parts.push(`${s.moved} moved`);
  if (s.resized > 0) parts.push(`${s.resized} resized`);
  if (parts.length > 0) return parts.join(" · ");
  return s.settings_changed > 0 ? "Tile settings updated" : "";
}

// Auto-refresh intervals in seconds; 0 = off
const REFRESH_INTERVALS = [
  { label: "Off", value: 0 },
  { label: "10s", value: 10 },
  { label: "30s", value: 30 },
  { label: "1m", value: 60 },
  { label: "5m", value: 300 },
  { label: "10m", value: 600 },
  { label: "30m", value: 1800 },
  { label: "1h", value: 3600 },
];

export function DashboardPage() {
  const { slug } = useParams<{ slug: string }>();
  const qc = useQueryClient();
  const { user: currentUser, clientConfig } = useAuth();
  const { hasAny, has } = usePermissions();

  const [editMode, setEditMode] = useState(false);
  const isDesktop = useMediaQuery("(min-width: 768px)");
  const [addWidgetOpen, setAddWidgetOpen] = useState(false);
  const [aiBarOpen, setAiBarOpen] = useState(false);
  const [highlightWidgetId, setHighlightWidgetId] = useState<number | null>(null);
  const [addedMessage, showAddedMessage] = useTransientMessage<string>(6000);
  const [editingName, setEditingName] = useState(false);
  const [nameInput, setNameInput] = useState("");
  const [nameError, setNameError] = useState<string | null>(null);
  const [publishError, setPublishError] = useState<string | null>(null);
  const [discardError, setDiscardError] = useState<string | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);

  const [refreshInterval, setRefreshInterval] = useState(0);
  const [refreshKey, setRefreshKey] = useState(0);

  const [isFullscreen, setIsFullscreen] = useState(false);

  const [dashboardParams, setDashboardParams] = useState<Record<string, ParamValue>>({});

  const [embedOpen, setEmbedOpen] = useState(false);
  const [shareOpen, setShareOpen] = useState(false);
  const [pendingDeleteWidgetId, setPendingDeleteWidgetId] = useState<number | null>(null);
  const [publishConfirmOpen, setPublishConfirmOpen] = useState(false);
  const [discardConfirmOpen, setDiscardConfirmOpen] = useState(false);
  const [unpublishConfirmOpen, setUnpublishConfirmOpen] = useState(false);
  const [unpublishError, setUnpublishError] = useState<string | null>(null);

  // Edit = live draft, View = published snapshot; keyed by mode so each caches separately. A
  // never-published dashboard has no snapshot, so the backend serves the draft to View too.
  const dashQ = useQuery({
    queryKey: ["dashboard", slug, editMode],
    queryFn: () => api.getDashboard(slug!, { draft: editMode }),
    enabled: slug != null,
  });

  const narrativesEnabled = clientConfig?.ai_dashboard_narratives === true;
  const dashboardId = dashQ.data?.id;
  const [narrativeExpanded, setNarrativeExpanded] = useState(false);
  const [narrativeError, setNarrativeError] = useState<NarrativeError | null>(null);

  // A 404 just means nothing has been generated yet; the panel stays hidden.
  const narrativeQ = useQuery({
    queryKey: ["dashboard-narrative", dashboardId],
    queryFn: () => api.getDashboardNarrative(dashboardId!),
    enabled: narrativesEnabled && dashboardId != null,
    retry: false,
  });
  const narrative: DashboardNarrative | null = narrativeQ.data?.narrative ?? null;

  const explainMutation = useMutation({
    mutationFn: () => api.explainDashboard(dashboardId!),
    onSuccess: (res) => {
      setNarrativeExpanded(true);
      if (res.status === "ok") {
        setNarrativeError(null);
        qc.setQueryData(["dashboard-narrative", dashboardId], { narrative: res.narrative });
      } else {
        setNarrativeError({ status: res.status, message: aiErrorMessage(res) });
      }
    },
    onError: (err) => {
      setNarrativeExpanded(true);
      setNarrativeError({ status: "error", message: apiErrorMessage(err, "Couldn't explain this dashboard.") });
    },
  });

  useDocumentTitle(
    dashQ.data ? `${dashQ.data.name || "Untitled dashboard"} · arcodash` : "Dashboard · arcodash",
  );

  const invalidateDashboardAndList = () =>
    Promise.all([
      qc.invalidateQueries({ queryKey: ["dashboard", slug] }),
      qc.invalidateQueries({ queryKey: ["dashboards"] }),
    ]);

  const publishMutation = useMutation({
    mutationFn: () => api.publishDashboard(slug!),
    onSuccess: async () => {
      setPublishConfirmOpen(false);
      setPublishError(null);
      await invalidateDashboardAndList();
    },
    onError: (err) => setPublishError(apiErrorMessage(err, "Failed to publish")),
  });

  const discardMutation = useMutation({
    mutationFn: () => api.discardDashboardChanges(slug!),
    onSuccess: async () => {
      setDiscardConfirmOpen(false);
      setDiscardError(null);
      await qc.invalidateQueries({ queryKey: ["dashboard", slug] });
    },
    onError: (err) => setDiscardError(apiErrorMessage(err, "Failed to discard changes")),
  });

  const unpublishMutation = useMutation({
    mutationFn: () => api.unpublishDashboard(slug!),
    onSuccess: async () => {
      setUnpublishConfirmOpen(false);
      setUnpublishError(null);
      await invalidateDashboardAndList();
    },
    onError: (err) => setUnpublishError(apiErrorMessage(err, "Failed to unpublish")),
  });

  // Don't re-send a rename the server already rejected on every blur; cleared when the field is
  // edited.
  const rejectedNameRef = useRef<string | null>(null);

  const renameMutation = useMutation({
    mutationFn: (name: string) => api.updateDashboard(slug!, { name }),
    onSuccess: async () => {
      setEditingName(false);
      setNameError(null);
      await qc.invalidateQueries({ queryKey: ["dashboard", slug] });
    },
    onError: (err) => {
      rejectedNameRef.current = nameInput.trim();
      setNameError(apiErrorMessage(err, "Failed to rename"));
    },
  });

  const favMutation = useMutation({
    mutationFn: (nextFavorite: boolean) =>
      nextFavorite ? api.starDashboard(dashQ.data!.id) : api.unstarDashboard(dashQ.data!.id),
    onSuccess: async () => {
      setActionError(null);
      await invalidateDashboardAndList();
    },
    onError: (err) => setActionError(apiErrorMessage(err, "Failed to update favorite")),
  });

  const deleteWidgetMutation = useMutation({
    mutationFn: (id: number) => api.deleteWidget(id),
    onSuccess: async () => {
      setActionError(null);
      setPendingDeleteWidgetId(null);
      await qc.invalidateQueries({ queryKey: ["dashboard", slug] });
    },
    onError: (err) => setActionError(apiErrorMessage(err, "Failed to delete widget")),
  });

  const tagsMutation = useMutation({
    mutationFn: (tags: string[]) => api.updateDashboard(slug!, { tags }),
    onSuccess: async () => {
      setActionError(null);
      await invalidateDashboardAndList();
    },
    onError: (err) => setActionError(apiErrorMessage(err, "Failed to update tags")),
  });

  useEffect(() => {
    if (refreshInterval === 0) return;
    const id = setInterval(() => {
      setRefreshKey((k) => k + 1);
      // refreshKey only re-runs each tile's result query; `widget.freshness` comes from this
      // dashboard fetch, so refetch it or the freshness dot freezes.
      qc.invalidateQueries({ queryKey: ["dashboard", slug] });
    }, refreshInterval * 1000);
    return () => clearInterval(id);
  }, [refreshInterval, qc, slug]);

  useEffect(() => {
    function onFsChange() {
      setIsFullscreen(document.fullscreenElement != null);
    }
    document.addEventListener("fullscreenchange", onFsChange);
    return () => document.removeEventListener("fullscreenchange", onFsChange);
  }, []);

  function toggleFullscreen() {
    if (!document.fullscreenElement) {
      document.documentElement.requestFullscreen().catch(console.error);
    } else {
      document.exitFullscreen().catch(console.error);
    }
  }

  useEffect(() => {
    if (!editMode) setAiBarOpen(false);
  }, [editMode]);

  useEffect(() => {
    if (highlightWidgetId == null) return;
    const t = setTimeout(() => setHighlightWidgetId(null), 1500);
    return () => clearTimeout(t);
  }, [highlightWidgetId]);

  function closeAiBar() {
    setAiBarOpen(false);
    requestAnimationFrame(() => {
      const trigger = document.querySelector<HTMLElement>('[data-action="add-with-ai"]');
      const target = trigger && trigger.offsetParent !== null ? trigger : document.querySelector<HTMLElement>('[aria-label="More actions"]');
      target?.focus();
    });
  }

  async function handleWidgetCreated(created: WidgetCreated) {
    setAiBarOpen(false);
    await qc.invalidateQueries({ queryKey: ["dashboard"] });
    const el = document.querySelector(`[data-widget-id="${created.widget_id}"]`);
    const reduced = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
    el?.scrollIntoView({ block: "center", behavior: reduced ? "auto" : "smooth" });
    setHighlightWidgetId(created.widget_id);
    showAddedMessage(
      created.row_count === 0
        ? "No rows yet — the widget will fill when data arrives"
        : "Added the widget at the bottom of the draft. Publish to share it.",
    );
  }

  if (dashQ.isLoading) return <PageLoader />;
  if (dashQ.isError)
    return <p className="text-destructive">{apiErrorMessage(dashQ.error, "Failed to load dashboard")}</p>;

  const dashboard = dashQ.data!;
  const canEdit = dashboard.caller.can_edit;
  // `can_share` mirrors `can_edit` server-side; object_role alone would let an owner without
  // edit_dashboard still manage sharing.
  const canManageSharing = dashboard.caller.can_share ?? false;
  const state = effectiveState(dashboard, canEdit ? "editor" : "viewer");
  // True for a never-published draft and for a dashboard unpublished after being live; both need
  // the same "publish it" affordances.
  const notCurrentlyPublished = !dashboard.published;
  const canDragTiles = isDesktop && dashboard.widgets.length > 0;
  const canPublish = dashboard.name.trim().length > 0 && dashboard.widgets.length > 0;
  // Nothing in the publish dialog itself can reveal why the button is greyed out — name it here.
  const publishBlockedReason =
    dashboard.widgets.length === 0
      ? "Add at least one widget to publish"
      : dashboard.name.trim().length === 0
        ? "Name the dashboard to publish"
        : undefined;

  function saveName() {
    if (renameMutation.isPending) return;
    const next = nameInput.trim();
    if (!next || next === dashboard.name) {
      setEditingName(false);
      return;
    }
    if (next !== rejectedNameRef.current) renameMutation.mutate(next);
  }

  const publishCopy = publishDialogCopy("dashboard", dashboard.visibility, notCurrentlyPublished);
  const publishSummaryLine =
    !notCurrentlyPublished && dashboard.pending_change_summary
      ? changeSummaryLine(dashboard.pending_change_summary)
      : "";

  const canExplain =
    narrativesEnabled &&
    !editMode &&
    !isFullscreen &&
    dashboard.widgets.some((w) => w.visualization_id != null);

  const aiAvailable = clientConfig?.ai_sql_generation === true && has("create_query");

  const primaryActions: ActionItem[] = [
    ...(canExplain
      ? [
          {
            key: "explain",
            label: "Explain",
            icon: Sparkles,
            variant: "outline",
            pending: explainMutation.isPending,
            pendingLabel: "Explaining…",
            onSelect: () => explainMutation.mutate(),
          } satisfies ActionItem,
        ]
      : []),
    ...(canEdit
      ? [
          {
            key: "edit-toggle",
            label: editMode ? "View" : "Edit",
            icon: editMode ? Eye : LayoutDashboard,
            variant: "outline",
            onSelect: () => setEditMode((e) => !e),
          } satisfies ActionItem,
        ]
      : []),
    ...(canEdit && editMode && aiAvailable
      ? [
          {
            key: "add-with-ai",
            label: "Add with AI",
            icon: Sparkles,
            variant: "outline",
            ariaExpanded: aiBarOpen,
            ariaControls: "ai-widget-bar",
            onSelect: () => setAiBarOpen((o) => !o),
          } satisfies ActionItem,
        ]
      : []),
    ...(canEdit && editMode
      ? [
          {
            key: "add-widget",
            label: "Add Widget",
            icon: Plus,
            onSelect: () => setAddWidgetOpen(true),
          } satisfies ActionItem,
        ]
      : []),
  ];

  const secondaryActions: ActionItem[] = [
    {
      key: "fullscreen",
      label: "Full screen",
      iconOnly: true,
      icon: isFullscreen ? Minimize2 : Maximize2,
      group: 1,
      onSelect: toggleFullscreen,
    },
    {
      key: "favorite",
      label: dashboard.is_favorite ? "Unfavorite" : "Favorite",
      iconOnly: true,
      group: 1,
      icon: (p) => (
        <Star
          {...p}
          className={cn(p.className, dashboard.is_favorite && "fill-yellow-400 text-yellow-400")}
        />
      ),
      onSelect: () => favMutation.mutate(!dashboard.is_favorite),
    },
    ...(canManageSharing
      ? [
          {
            key: "share",
            label: "Share",
            icon: Share2,
            group: 2,
            disabled: notCurrentlyPublished,
            title: notCurrentlyPublished ? "Publish this dashboard first" : undefined,
            onSelect: () => setShareOpen(true),
          } satisfies ActionItem,
        ]
      : []),
    {
      key: "embed",
      label: "Public link",
      icon: Link2,
      group: 2,
      disabled: notCurrentlyPublished,
      title: notCurrentlyPublished ? "Publish this dashboard first" : undefined,
      onSelect: () => setEmbedOpen(true),
    },
    ...(canEdit && !notCurrentlyPublished
      ? [
          {
            key: "unpublish",
            label: "Unpublish",
            pending: unpublishMutation.isPending,
            pendingLabel: "Unpublishing…",
            group: 3,
            onSelect: () => setUnpublishConfirmOpen(true),
          } satisfies ActionItem,
        ]
      : []),
  ];

  const refreshSelect = (
    <Select value={String(refreshInterval)} onValueChange={(v) => setRefreshInterval(Number(v))}>
      <SelectTrigger className="h-8 text-xs">
        <RefreshCw className="mr-1 h-3.5 w-3.5 shrink-0" />
        <SelectValue>{`Refresh: ${REFRESH_INTERVALS.find((r) => r.value === refreshInterval)?.label}`}</SelectValue>
      </SelectTrigger>
      <SelectContent>
        {REFRESH_INTERVALS.map((r) => (
          <SelectItem key={r.value} value={String(r.value)}>
            {r.label}
          </SelectItem>
        ))}
      </SelectContent>
    </Select>
  );

  // Same refresh choices in the "More actions" menu; no leading DropdownMenuSeparator, since
  // ActionBar already renders one before menuExtra.
  const refreshMenuExtra = (
    <>
      <DropdownMenuLabel>Refresh</DropdownMenuLabel>
      {REFRESH_INTERVALS.map((r) => (
        <DropdownMenuItem key={r.value} onSelect={() => setRefreshInterval(r.value)}>
          {r.value === refreshInterval ? <Check className="size-4" /> : <span className="size-4" />}
          {r.label}
        </DropdownMenuItem>
      ))}
    </>
  );

  return (
    <div className="space-y-4">
      {!isFullscreen && (
        <div className="flex flex-col gap-2">
          <BackButton to="/dashboards" label="Back to dashboards" />
          <div className="flex min-w-0 flex-wrap items-center gap-3">
            {editingName && canEdit ? (
              <div className="flex flex-col gap-1">
                <Input
                  autoFocus
                  className="h-8 w-full max-w-56 text-lg font-semibold"
                  value={nameInput}
                  onChange={(e) => {
                    rejectedNameRef.current = null;
                    setNameInput(e.target.value);
                  }}
                  onBlur={saveName}
                  onKeyDown={(e) => {
                    if (e.key === "Enter") saveName();
                    if (e.key === "Escape") {
                      setNameError(null);
                      setEditingName(false);
                    }
                  }}
                />
                {nameError && <p className="text-sm text-destructive">{nameError}</p>}
              </div>
            ) : canEdit ? (
              <button
                className="flex cursor-pointer items-center gap-1 text-2xl font-semibold hover:underline"
                onClick={() => {
                  setNameInput(dashboard.name);
                  setNameError(null);
                  setEditingName(true);
                }}
                type="button"
              >
                {dashboard.name}
                <Pencil className="h-4 w-4 text-muted-foreground" />
              </button>
            ) : (
              <span className="text-2xl font-semibold">{dashboard.name}</span>
            )}

            <OwnerChip
              name={dashboard.owner_name ?? "—"}
              isCurrentUser={dashboard.user_id === currentUser?.id}
              showName
            />

            {canEdit && (
              <div className="flex items-center gap-1.5">
                <Badge variant={state.badge === "Draft" ? "secondary" : "default"}>
                  {state.badge}
                </Badge>
                {state.pending && <UnpublishedBadge />}
              </div>
            )}

            <TagEditor
              tags={dashboard.tags}
              editable={editMode}
              pending={tagsMutation.isPending}
              onChange={(tags) => tagsMutation.mutate(tags)}
            />
          </div>

          {actionError && <p className="text-sm text-destructive">{actionError}</p>}

          <ActionBar
            primary={primaryActions}
            secondary={secondaryActions}
            collapseAt="36rem"
            extra={refreshSelect}
            menuExtra={refreshMenuExtra}
          />

          {canEdit && !editMode && state.pending && (
            <p className="text-xs text-muted-foreground">
              You're viewing the published version. Switch to Edit to see your draft.
            </p>
          )}
        </div>
      )}

      {isFullscreen && (
        <button
          type="button"
          onClick={toggleFullscreen}
          title="Exit full screen"
          aria-label="Exit full screen"
          className="fixed top-4 right-4 z-50 flex h-9 w-9 cursor-pointer items-center justify-center rounded-full border border-border bg-card text-foreground shadow-lg hover:bg-muted"
        >
          <Minimize2 className="h-4 w-4" />
        </button>
      )}

      <DashboardParamsBar widgets={dashboard.widgets} onParamsChange={setDashboardParams} />

      {canEdit && editMode && (
        <div
          className={`flex flex-wrap items-center justify-between gap-2 rounded-md border px-3 py-2 text-sm ${
            notCurrentlyPublished || state.pending
              ? "border-warning/30 bg-warning/10 text-warning-foreground"
              : "border-border bg-muted/50 text-muted-foreground"
          }`}
        >
          <p>
            <span role="status" aria-live="polite" className={addedMessage ? "" : "sr-only"}>
              {addedMessage}
            </span>
            {addedMessage ? null : !notCurrentlyPublished && !state.pending ? (
              <>
                {canDragTiles ? "Drag a tile by its header to move it, or by its corner to resize. " : ""}
                Changes are saved as a draft.
              </>
            ) : notCurrentlyPublished ? (
              <>
                <strong>This dashboard is a draft.</strong> It isn't visible to anyone yet.
              </>
            ) : (
              <>
                <strong>You're editing a draft.</strong> Your workspace still sees the published
                version.
              </>
            )}
          </p>
          <div className="flex items-center gap-2">
            {!notCurrentlyPublished && !state.pending ? (
              <Button size="sm" disabled title="No changes to publish">
                Publish changes
              </Button>
            ) : (
              <>
                <Button
                  size="sm"
                  disabled={!canPublish}
                  title={!canPublish ? publishBlockedReason : undefined}
                  onClick={() => setPublishConfirmOpen(true)}
                >
                  {notCurrentlyPublished ? "Publish" : "Publish changes"}
                </Button>
                {!notCurrentlyPublished && (
                  <Button size="sm" variant="outline" onClick={() => setDiscardConfirmOpen(true)}>
                    Discard changes
                  </Button>
                )}
              </>
            )}
          </div>
        </div>
      )}

      {canEdit && editMode && aiAvailable && aiBarOpen && (
        <AddWithAiBar
          dashboardId={dashboard.id}
          widgets={dashboard.widgets}
          onClose={closeAiBar}
          onCreated={handleWidgetCreated}
        />
      )}

      {narrativesEnabled &&
        !editMode &&
        (narrative != null || explainMutation.isPending || narrativeError != null) && (
          <DashboardNarrativePanel
            widgets={dashboard.widgets}
            narrative={narrative}
            expanded={narrativeExpanded || narrative == null}
            onToggle={() => setNarrativeExpanded((e) => !e)}
            pending={explainMutation.isPending}
            error={narrativeError}
            onRegenerate={() => explainMutation.mutate()}
            isAdmin={hasAny(["admin"])}
          />
        )}

      {dashboard.widgets.length === 0 ? (
        canEdit ? (
          <EmptyState
            icon={LayoutDashboard}
            iconClassName="bg-indigo-50 text-indigo-600"
            title="Add your first widget"
            description="Widgets turn a saved query into a chart or table on this dashboard."
            action={
              editMode ? (
                <div className="flex items-center gap-2">
                  {aiAvailable && (
                    <Button variant="outline" onClick={() => setAiBarOpen(true)}>
                      <Sparkles className="mr-1 h-4 w-4" /> Add with AI
                    </Button>
                  )}
                  <Button onClick={() => setAddWidgetOpen(true)}>
                    <Plus className="mr-1 h-4 w-4" /> Add Widget
                  </Button>
                </div>
              ) : (
                <Button variant="outline" onClick={() => setEditMode(true)}>
                  Switch to Edit mode to add widgets
                </Button>
              )
            }
          />
        ) : (
          <p className="text-sm text-muted-foreground">No widgets have been added to this dashboard yet.</p>
        )
      ) : (
        <DashboardGrid
          widgets={dashboard.widgets}
          editMode={editMode}
          onDeleteWidget={setPendingDeleteWidgetId}
          dashboardParams={dashboardParams}
          refreshKey={refreshKey}
          dashboardSlug={slug}
          highlightWidgetId={highlightWidgetId}
        />
      )}

      <AddWidgetDialog
        dashboardId={dashboard.id}
        open={addWidgetOpen}
        onClose={() => setAddWidgetOpen(false)}
        widgets={dashboard.widgets}
      />

      <ConfirmDialog
        open={pendingDeleteWidgetId != null}
        onOpenChange={(open) => !open && setPendingDeleteWidgetId(null)}
        title="Remove this widget?"
        confirmLabel="Remove"
        destructive
        confirmPending={deleteWidgetMutation.isPending}
        onConfirm={() => pendingDeleteWidgetId != null && deleteWidgetMutation.mutate(pendingDeleteWidgetId)}
      />

      <Dialog
        open={publishConfirmOpen}
        onOpenChange={(open) => {
          setPublishConfirmOpen(open);
          if (!open) setPublishError(null);
        }}
      >
        <DialogContent>
          <DialogHeader>
            <DialogTitle>{publishCopy.title}</DialogTitle>
          </DialogHeader>
          <p className="text-sm text-muted-foreground">{publishCopy.body}</p>
          {publishSummaryLine && <p className="text-sm text-muted-foreground">{publishSummaryLine}</p>}
          {publishError && <p className="text-sm text-destructive">{publishError}</p>}
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

      <Dialog
        open={discardConfirmOpen}
        onOpenChange={(open) => {
          setDiscardConfirmOpen(open);
          if (!open) setDiscardError(null);
        }}
      >
        <DialogContent>
          <DialogHeader>
            <DialogTitle>Discard your unpublished changes?</DialogTitle>
          </DialogHeader>
          <p className="text-sm text-muted-foreground">
            This restores the dashboard to the published version. This can't be undone.
          </p>
          {discardError && <p className="text-sm text-destructive">{discardError}</p>}
          <DialogFooter>
            <Button variant="outline" onClick={() => setDiscardConfirmOpen(false)}>
              Keep editing
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
        onOpenChange={(open) => {
          setUnpublishConfirmOpen(open);
          if (!open) setUnpublishError(null);
        }}
        title="Unpublish this dashboard?"
        description="It goes back to draft. Only you and the people you've shared it with will see it."
        confirmLabel="Unpublish"
        confirmPending={unpublishMutation.isPending}
        onConfirm={() => unpublishMutation.mutate()}
        error={unpublishError}
        body={
          <ul className="list-disc space-y-1 pl-5 text-sm text-muted-foreground">
            <li>Your workspace loses access until you publish again.</li>
            <li>
              Public links and embeds stop working. They keep their tokens and start working
              again when you publish.
            </li>
            <li>Scheduled refreshes stop running.</li>
            <li>"Discard changes" stops being available — there's nothing published to revert to.</li>
          </ul>
        }
      />

      {embedOpen && (
        <ShareEmbedDialog
          objectType="dashboard"
          objectId={dashboard.id}
          resourceTitle={dashboard.name}
          visibility={dashboard.visibility}
          onClose={() => setEmbedOpen(false)}
        />
      )}

      <ShareDialog
        objectType="dashboard"
        objectId={dashboard.id}
        ownerId={dashboard.user_id}
        visibility={dashboard.visibility}
        open={shareOpen}
        canManage={canManageSharing}
        onClose={() => setShareOpen(false)}
        onChanged={() => dashQ.refetch()}
      />
    </div>
  );
}
