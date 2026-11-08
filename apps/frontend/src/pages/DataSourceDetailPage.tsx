import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Link, useNavigate, useParams } from "react-router";
import { Edit, SearchX, Trash2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { ConfirmDialog } from "@/components/ui/confirm-dialog";
import { Switch } from "@/components/ui/switch";
import { DataSourceAiProfileCard } from "@/components/data-sources/DataSourceAiProfileCard";
import { DataQualityChecksCard } from "@/components/data-sources/DataQualityChecksCard";
import { HealthIndicator } from "@/components/data-sources/DataSourceList";
import { SchemaBrowser } from "@/components/data-sources/SchemaBrowser";
import { TestButton } from "@/components/data-sources/TestButton";
import { BackButton } from "@/components/nav/BackButton";
import { DocsLink } from "@/components/common/DocsLink";
import { EmptyState } from "@/components/common/EmptyState";
import { Loader, PageLoader } from "@/components/ui/loader";
import { ApiError, apiErrorMessage, api } from "@/lib/api";
import { usePermissions } from "@/lib/permissions";
import { exactTs } from "@/lib/scheduling";
import { useDocumentTitle } from "@/lib/useDocumentTitle";

/** Reverse of `GroupsPage.tsx`'s `ManageGroupDialog` "Data sources" panel — this reads/writes
 * the same `data_source_group` join, just keyed from the data source side ("who can reach me?"
 * instead of "what can I reach?"). Mutations reuse the existing group-keyed assign/remove
 * endpoints (they already take a `data_source_id`), so there's no new write surface here. */
function GroupAccessSection({ dataSourceId }: { dataSourceId: number }) {
  const { has, isLoading: permissionsLoading } = usePermissions();
  const queryClient = useQueryClient();
  const canView = has("view_groups");
  const canManage = has("manage_groups");

  const groupsQuery = useQuery({
    queryKey: ["data-source-groups", dataSourceId],
    queryFn: () => api.listDataSourceGroups(dataSourceId),
    enabled: canView,
  });

  const invalidate = () =>
    queryClient.invalidateQueries({ queryKey: ["data-source-groups", dataSourceId] });
  const [accessError, setAccessError] = useState<string | null>(null);
  const setAccess = useMutation({
    mutationFn: ({ groupId, grant }: { groupId: number; grant: boolean }) =>
      grant
        ? api.assignGroupDataSource(groupId, { data_source_id: dataSourceId })
        : api.removeGroupDataSource(groupId, dataSourceId),
    onSuccess: () => {
      setAccessError(null);
      invalidate();
    },
    onError: (err, { grant }) =>
      setAccessError(apiErrorMessage(err, grant ? "Failed to grant group access" : "Failed to remove group access")),
  });

  if (permissionsLoading || !canView) return null;

  return (
    <div>
      <p className="mb-2 text-sm font-medium">Group access</p>
      <p className="mb-2 text-xs text-muted-foreground">
        Which groups can see and query this data source. Unrestricted groups already reach every
        data source; toggle a restricted group on to add this one to its allowlist.
      </p>

      {groupsQuery.isLoading && <Loader size={16} />}
      {groupsQuery.isError && (
        <p className="text-sm text-destructive">
          {apiErrorMessage(groupsQuery.error, "Failed to load group access")}
        </p>
      )}

      {groupsQuery.data && (
        <ul className="space-y-1 rounded-md border border-border p-3">
          {groupsQuery.data.length === 0 && (
            <li className="text-sm text-muted-foreground">No groups in this workspace yet.</li>
          )}
          {groupsQuery.data.map((g) => {
            const toggleId = `ds-group-access-${g.group_id}`;
            return (
              <li key={g.group_id} className="flex items-center justify-between text-sm">
                <span>{g.group_name}</span>
                {g.restricted ? (
                  <span className="flex items-center gap-2">
                    <label htmlFor={toggleId} className="text-xs text-muted-foreground">
                      Restricted
                    </label>
                    <Switch
                      id={toggleId}
                      checked={g.has_access}
                      disabled={!canManage || setAccess.isPending}
                      onCheckedChange={(grant) => setAccess.mutate({ groupId: g.group_id, grant })}
                    />
                  </span>
                ) : (
                  <span className="text-xs text-muted-foreground">All data sources</span>
                )}
              </li>
            );
          })}
        </ul>
      )}
      {accessError && <p className="mt-2 text-sm text-destructive">{accessError}</p>}
    </div>
  );
}

export function DataSourceDetailPage() {
  const params = useParams<{ id: string }>();
  const id = Number(params.id);
  const queryClient = useQueryClient();
  const navigate = useNavigate();
  const [deleteConfirmOpen, setDeleteConfirmOpen] = useState(false);
  const [deleteError, setDeleteError] = useState<string | null>(null);

  const dataSourceQuery = useQuery({
    queryKey: ["data-source", id],
    queryFn: () => api.getDataSource(id),
    enabled: Number.isFinite(id),
  });

  useDocumentTitle(
    dataSourceQuery.data ? `${dataSourceQuery.data.name || "Untitled data source"} · arcodash` : "Data Source · arcodash",
  );

  const [pauseResumeError, setPauseResumeError] = useState<string | null>(null);

  const togglePause = useMutation({
    mutationFn: (pause: boolean) => (pause ? api.pauseDataSource(id) : api.resumeDataSource(id)),
    onSuccess: () => {
      setPauseResumeError(null);
      queryClient.invalidateQueries({ queryKey: ["data-source", id] });
    },
    onError: (err, pause) =>
      setPauseResumeError(
        apiErrorMessage(err, pause ? "Failed to turn off this data source" : "Failed to turn on this data source"),
      ),
  });

  const deleteMutation = useMutation({
    mutationFn: () => api.deleteDataSource(id),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["data-sources"] });
      navigate("/data_sources");
    },
    onError: (err) => setDeleteError(apiErrorMessage(err, "Failed to delete data source")),
  });

  if (dataSourceQuery.isLoading) {
    return <PageLoader />;
  }

  if (dataSourceQuery.isError || !dataSourceQuery.data) {
    const isNotFound =
      !Number.isFinite(id) ||
      (dataSourceQuery.error instanceof ApiError && dataSourceQuery.error.status === 404);
    return (
      <div className="max-w-2xl">
        <EmptyState
          icon={SearchX}
          iconClassName="bg-destructive/10 text-destructive"
          title={isNotFound ? "Data source not found" : "Couldn't load this data source"}
          description={
            isNotFound
              ? "This data source doesn't exist or you don't have access to it."
              : apiErrorMessage(dataSourceQuery.error, "Something went wrong loading this data source.")
          }
          action={
            <Button asChild variant="outline">
              <Link to="/data_sources">Back to data sources</Link>
            </Button>
          }
        />
      </div>
    );
  }

  const ds = dataSourceQuery.data;
  const canEdit = ds.caller.can_edit;

  return (
    <div className="space-y-8">
      <BackButton to="/data_sources" label="Back to data sources" />
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <div className="flex items-center gap-3">
            <h1 className="text-2xl font-semibold">{ds.name}</h1>
            <HealthIndicator ds={ds} />
          </div>
          <DocsLink topic="dataSourceHealth" label="connection health" />
        </div>
        {canEdit && (
          <div className="flex flex-wrap gap-2">
            <Button variant="outline" onClick={() => togglePause.mutate(!ds.paused)} disabled={togglePause.isPending}>
              {togglePause.isPending ? (
                <>
                  <Loader size={16} />
                  {ds.paused ? "Turning on…" : "Turning off…"}
                </>
              ) : ds.paused ? (
                "Turn on"
              ) : (
                "Turn off"
              )}
            </Button>
            <Button variant="outline" asChild>
              <Link to={`/data_sources/${ds.id}/edit`}>
                <Edit className="w-3.5 h-3.5 mr-1" /> Edit
              </Link>
            </Button>
            <Button
              variant="outline"
              className="text-destructive hover:text-destructive"
              onClick={() => {
                setDeleteError(null);
                setDeleteConfirmOpen(true);
              }}
            >
              <Trash2 className="w-3.5 h-3.5 mr-1" /> Delete
            </Button>
          </div>
        )}
      </div>

      {pauseResumeError && <p className="text-sm text-destructive">{pauseResumeError}</p>}

      {!ds.paused && ds.health_state === "failing" && (
        <div className="rounded-md border border-destructive/30 bg-destructive/5 p-3 text-sm">
          <p className="font-medium text-destructive">Can&apos;t connect</p>
          {ds.health_error && <p className="mt-1 text-muted-foreground">{ds.health_error}</p>}
          {ds.health_checked_at && (
            <p className="mt-1 text-xs text-muted-foreground">
              Last checked {exactTs(Date.parse(ds.health_checked_at))}
            </p>
          )}
        </div>
      )}

      <dl className="grid grid-cols-1 sm:grid-cols-2 gap-4 text-sm">
        <div>
          <dt className="text-muted-foreground">Type</dt>
          <dd>{ds.type_display_name}</dd>
        </div>
        <div>
          <dt className="text-muted-foreground">Created</dt>
          <dd>{exactTs(Date.parse(ds.created_at))}</dd>
        </div>
      </dl>

      {canEdit && (
        <div>
          <p className="mb-2 text-sm font-medium">Connection</p>
          <TestButton target={{ id: ds.id }} />
        </div>
      )}

      <GroupAccessSection dataSourceId={ds.id} />

      <DataQualityChecksCard dataSourceId={ds.id} />

      <DataSourceAiProfileCard
        dataSourceId={ds.id}
        typeDisplayName={ds.type_display_name}
        schemaRefreshedAt={ds.schema_refreshed_at}
      />

      <div className="rounded-md border border-border p-4">
        <SchemaBrowser dataSourceId={ds.id} />
      </div>

      <ConfirmDialog
        open={deleteConfirmOpen}
        onOpenChange={(open) => {
          setDeleteConfirmOpen(open);
          if (!open) setDeleteError(null);
        }}
        title="Delete data source?"
        description={`This will permanently delete "${ds.name}". Data sources still used by a query can't be deleted.`}
        confirmLabel="Delete"
        destructive
        confirmPending={deleteMutation.isPending}
        onConfirm={() => {
          setDeleteError(null);
          deleteMutation.mutate();
        }}
        error={deleteError}
      />
    </div>
  );
}
