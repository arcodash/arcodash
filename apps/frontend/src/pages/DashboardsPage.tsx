import { useState, type FormEvent } from "react";
import { useResizableColumn } from "@/hooks/useResizableColumn";
import { ColumnResizeHandle } from "@/components/common/ColumnResizeHandle";
import { keepPreviousData, useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Link, useNavigate } from "react-router";
import { LayoutDashboard, Plus, Star, Trash2 } from "lucide-react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardFooter, CardHeader, CardTitle } from "@/components/ui/card";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { ConfirmDialog } from "@/components/ui/confirm-dialog";
import { Dialog, DialogContent, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog";
import { PageHeader } from "@/components/common/PageHeader";
import { EmptyState } from "@/components/common/EmptyState";
import { ListToolbar, ownerChipGroup, TagFilterPills } from "@/components/common/ListToolbar";
import { OwnerChip } from "@/components/sharing/OwnerChip";
import { VisibilityChip } from "@/components/sharing/VisibilityChip";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { Input } from "@/components/ui/input";
import { PageLoader } from "@/components/ui/loader";
import { api, apiErrorMessage } from "@/lib/api";
import { useDebouncedValue } from "@/lib/useDebouncedValue";
import { useListView } from "@/lib/useListView";
import { usePermissions } from "@/lib/permissions";
import type { DashboardListItem, ScopeFilter } from "@/types/api";

/** Favorite toggle + Delete — identical markup in the table row and the card footer, so
 * switching view never changes what a row can do. */
function DashboardActions({
  dashboard: d,
  onDeleteClick,
}: {
  dashboard: DashboardListItem;
  onDeleteClick: (d: DashboardListItem) => void;
}) {
  const qc = useQueryClient();
  const [favError, setFavError] = useState<string | null>(null);
  const favMutation = useMutation({
    mutationFn: () => (d.is_favorite ? api.unstarDashboard(d.id) : api.starDashboard(d.id)),
    onSuccess: () => {
      setFavError(null);
      qc.invalidateQueries({ queryKey: ["dashboards"] });
    },
    onError: (err) => setFavError(apiErrorMessage(err, "Failed to update favorite")),
  });

  return (
    <div
      className="flex flex-col items-end gap-0.5"
      onClick={(e) => e.stopPropagation()}
      onKeyDown={(e) => e.stopPropagation()}
    >
      <div className="flex items-center gap-1">
        <Button
          variant="ghost"
          size="icon-sm"
          className="h-7 w-7"
          title={d.is_favorite ? "Remove from favorites" : "Add to favorites"}
          aria-label={d.is_favorite ? "Remove from favorites" : "Add to favorites"}
          disabled={favMutation.isPending}
          onClick={() => favMutation.mutate()}
        >
          <Star className={`h-3.5 w-3.5 ${d.is_favorite ? "fill-yellow-400 text-yellow-400" : ""}`} />
        </Button>
        {d.caller.can_edit && (
          <Button
            variant="ghost"
            size="icon-sm"
            className="h-7 w-7 text-destructive hover:text-destructive"
            title="Delete"
            onClick={() => onDeleteClick(d)}
          >
            <Trash2 className="h-3.5 w-3.5" />
          </Button>
        )}
      </div>
      {favError && <div className="text-xs text-destructive">{favError}</div>}
    </div>
  );
}

function DashboardChips({ d }: { d: DashboardListItem }) {
  return (
    <>
      <VisibilityChip visibility={d.visibility} objectType="dashboard" isDraft={!d.published} />
      {d.is_archived && (
        <Badge variant="outline" className="text-muted-foreground">
          Archived
        </Badge>
      )}
    </>
  );
}

function DashboardTags({ tags }: { tags: string[] }) {
  if (tags.length === 0) return null;
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

interface RowProps {
  dashboard: DashboardListItem;
  onDeleteClick: (d: DashboardListItem) => void;
}

function DashboardTableRow({ dashboard: d, onDeleteClick }: RowProps) {
  const navigate = useNavigate();
  return (
    <TableRow onClick={() => navigate(`/dashboard/${d.slug}`)} className="cursor-pointer border-t border-border hover:bg-muted/30">
      <TableCell className="w-full max-w-0">
        <div className="flex min-w-0 flex-col gap-1">
          <div className="flex min-w-0 flex-wrap items-center gap-2">
            <div className="min-w-0 flex-1">
              <Tooltip>
                <TooltipTrigger asChild>
                  <Link
                    to={`/dashboard/${d.slug}`}
                    className="block truncate text-sm font-medium hover:underline"
                    onClick={(e) => e.stopPropagation()}
                  >
                    {d.name}
                  </Link>
                </TooltipTrigger>
                <TooltipContent>{d.name}</TooltipContent>
              </Tooltip>
            </div>
            <div className="flex shrink-0 items-center gap-2">
              <DashboardChips d={d} />
            </div>
          </div>
          <DashboardTags tags={d.tags} />
        </div>
      </TableCell>
      <TableCell className="hidden @min-[71rem]/list:table-cell w-px whitespace-nowrap text-muted-foreground">
        <OwnerChip name={d.owner_name ?? "—"} showName />
      </TableCell>
      <TableCell className="hidden @min-[68rem]/list:table-cell w-px whitespace-nowrap text-muted-foreground">
        {new Date(d.updated_at).toLocaleDateString()}
      </TableCell>
      <TableCell className="w-px whitespace-nowrap">
        <DashboardActions dashboard={d} onDeleteClick={onDeleteClick} />
      </TableCell>
    </TableRow>
  );
}

function DashboardCard({ dashboard: d, onDeleteClick }: RowProps) {
  const navigate = useNavigate();
  return (
    <Card
      className="cursor-pointer transition-colors hover:bg-muted/50"
      onClick={() => navigate(`/dashboard/${d.slug}`)}
    >
      <CardHeader className="pb-3">
        <Link to={`/dashboard/${d.slug}`} onClick={(e) => e.stopPropagation()}>
          <CardTitle className="text-base hover:underline">{d.name}</CardTitle>
        </Link>
        <div className="flex flex-wrap items-center gap-2">
          <OwnerChip name={d.owner_name ?? "—"} showName />
          <DashboardChips d={d} />
        </div>
        <DashboardTags tags={d.tags} />
      </CardHeader>
      <CardContent className="text-xs text-muted-foreground">
        Updated {new Date(d.updated_at).toLocaleDateString()}
      </CardContent>
      <CardFooter className="border-t border-border pt-3">
        <DashboardActions dashboard={d} onDeleteClick={onDeleteClick} />
      </CardFooter>
    </Card>
  );
}

export function DashboardsPage() {
  const nameCol = useResizableColumn("arcodash.listcol.dashboards.name");
  const qc = useQueryClient();
  const navigate = useNavigate();
  const { has } = usePermissions();
  const canCreate = has("create_dashboard");
  const [search, setSearch] = useState("");
  const debouncedSearch = useDebouncedValue(search, 250);
  const [creating, setCreating] = useState(false);
  const [favOnly, setFavOnly] = useState(false);
  const [activeTag, setActiveTag] = useState<string | null>(null);
  const [scope, setScope] = useState<ScopeFilter>("all");
  const [dialogOpen, setDialogOpen] = useState(false);
  const [newName, setNewName] = useState("");
  const [createError, setCreateError] = useState<string | null>(null);
  const [showArchived, setShowArchived] = useState(false);
  const [pendingDelete, setPendingDelete] = useState<DashboardListItem | null>(null);
  const [deleteError, setDeleteError] = useState<string | null>(null);
  const { view, setView, isMobile } = useListView("list-view:dashboards");

  // Pass search + scope to API; tag/favorite filters are client-side since the list is usually
  // small.
  const dashQ = useQuery({
    queryKey: ["dashboards", debouncedSearch, scope, showArchived],
    queryFn: () =>
      api.listDashboards(showArchived, debouncedSearch || undefined, scope === "all" ? undefined : scope),
    placeholderData: keepPreviousData,
  });

  const tagsQ = useQuery({
    queryKey: ["dashboard-tags"],
    queryFn: () => api.getDashboardTags(),
  });

  const allTags = tagsQ.data?.tags ?? [];

  const filtered = dashQ.data?.filter(
    (d) => (!favOnly || d.is_favorite) && (!activeTag || d.tags.includes(activeTag)),
  );
  const filtersActive = search !== "" || favOnly || activeTag !== null || scope !== "all" || showArchived;

  const deleteMutation = useMutation({
    mutationFn: (slug: string) => api.deleteDashboard(slug),
    onSuccess: (_data, slug) => {
      qc.invalidateQueries({ queryKey: ["dashboards"] });
      qc.removeQueries({ queryKey: ["dashboard", slug] });
      setPendingDelete(null);
    },
    onError: (err) => setDeleteError(apiErrorMessage(err, "Failed to delete dashboard")),
  });

  function openNewDialog() {
    setNewName("");
    setCreateError(null);
    setDialogOpen(true);
  }

  async function handleCreate(e: FormEvent) {
    e.preventDefault();
    const name = newName.trim();
    if (!name) return;
    setCreating(true);
    try {
      const d = await api.createDashboard(name);
      await qc.invalidateQueries({ queryKey: ["dashboards"] });
      setDialogOpen(false);
      navigate(`/dashboard/${d.slug}`);
    } catch (e) {
      setCreateError(apiErrorMessage(e, "Failed to create dashboard"));
    } finally {
      setCreating(false);
    }
  }

  const chipGroups = [ownerChipGroup(scope, setScope)];
  const activeFilterCount = (scope !== "all" ? 1 : 0) + (favOnly ? 1 : 0) + (showArchived ? 1 : 0);

  function onDeleteClick(d: DashboardListItem) {
    setDeleteError(null);
    setPendingDelete(d);
  }

  return (
    <div className="space-y-6">
      <PageHeader
        title="Dashboards"
        action={
          canCreate ? (
            <Button size="sm" onClick={openNewDialog}>
              <Plus className="w-3 h-3" />
              New Dashboard
            </Button>
          ) : undefined
        }
      />

      <ListToolbar
        search={search}
        onSearchChange={setSearch}
        searchPlaceholder="Search dashboards…"
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
        isFetching={dashQ.isFetching && !dashQ.isLoading}
        view={view}
        onViewChange={setView}
        showViewToggle={!isMobile}
      />

      <TagFilterPills tags={allTags} active={activeTag} onChange={setActiveTag} />

      {dashQ.isLoading && <PageLoader />}

      {dashQ.isError && (
        <p className="text-destructive">{apiErrorMessage(dashQ.error, "Failed to load dashboards")}</p>
      )}

      {filtered && filtered.length === 0 && filtersActive && (
        <EmptyState
          icon={LayoutDashboard}
          iconClassName="bg-violet-50 text-violet-600"
          title="No dashboards match these filters"
          description="Try adjusting or clearing your search and filters."
          action={
            <Button
              size="sm"
              variant="outline"
              onClick={() => {
                setSearch("");
                setFavOnly(false);
                setActiveTag(null);
                setScope("all");
                setShowArchived(false);
              }}
            >
              Clear filters
            </Button>
          }
        />
      )}

      {filtered && filtered.length === 0 && !filtersActive && (
        <EmptyState
          icon={LayoutDashboard}
          iconClassName="bg-violet-50 text-violet-600"
          title="Build your first dashboard"
          description="Pin query results into one shared view your team can read at a glance."
          action={
            canCreate && (
              <Button size="sm" onClick={openNewDialog}>
                <Plus className="w-3 h-3" />
                Create Your First Dashboard
              </Button>
            )
          }
        />
      )}

      {filtered && filtered.length > 0 && view === "table" && (
        <div className="@container/list">
          <Table className="w-full">
            <TableHeader className="bg-muted/50">
              <TableRow>
                <TableHead ref={nameCol.headRef} style={{ minWidth: nameCol.width ?? undefined }} className="relative w-full max-w-0 px-4 py-3 text-xs font-medium">
                  Name
                  <ColumnResizeHandle column={nameCol} />
                </TableHead>
                <TableHead className="hidden @min-[71rem]/list:table-cell w-px whitespace-nowrap px-4 py-3 text-xs font-medium">
                  Owner
                </TableHead>
                <TableHead className="hidden @min-[68rem]/list:table-cell w-px whitespace-nowrap px-4 py-3 text-xs font-medium">
                  Updated
                </TableHead>
                <TableHead className="w-px whitespace-nowrap px-4 py-3 text-xs font-medium">
                  <span className="sr-only">Actions</span>
                </TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {filtered.map((d) => (
                <DashboardTableRow key={d.id} dashboard={d} onDeleteClick={onDeleteClick} />
              ))}
            </TableBody>
          </Table>
        </div>
      )}

      {filtered && filtered.length > 0 && view === "cards" && (
        <div className="grid grid-cols-1 gap-4 md:grid-cols-2 lg:grid-cols-3">
          {filtered.map((d) => (
            <DashboardCard key={d.id} dashboard={d} onDeleteClick={onDeleteClick} />
          ))}
        </div>
      )}

      <Dialog
        open={dialogOpen}
        onOpenChange={(open) => {
          setDialogOpen(open);
          if (!open) setCreateError(null);
        }}
      >
        <DialogContent>
          <DialogHeader>
            <DialogTitle>New Dashboard</DialogTitle>
          </DialogHeader>
          <form onSubmit={handleCreate} className="space-y-4">
            <Input
              autoFocus
              placeholder="Dashboard name"
              value={newName}
              onChange={(e) => setNewName(e.target.value)}
            />
            {createError && <p className="text-sm text-destructive">{createError}</p>}
            <DialogFooter>
              <Button type="button" variant="outline" onClick={() => setDialogOpen(false)}>
                Cancel
              </Button>
              <Button type="submit" disabled={!newName.trim() || creating}>
                {creating ? "Creating…" : "Create"}
              </Button>
            </DialogFooter>
          </form>
        </DialogContent>
      </Dialog>

      <ConfirmDialog
        open={pendingDelete !== null}
        onOpenChange={(open) => {
          if (open) return;
          setPendingDelete(null);
          setDeleteError(null);
        }}
        title="Delete dashboard?"
        description={`This will permanently delete "${pendingDelete?.name ?? ""}".`}
        confirmLabel="Delete"
        destructive
        confirmPending={deleteMutation.isPending}
        onConfirm={() => pendingDelete && deleteMutation.mutate(pendingDelete.slug)}
        error={deleteError}
      />
    </div>
  );
}
