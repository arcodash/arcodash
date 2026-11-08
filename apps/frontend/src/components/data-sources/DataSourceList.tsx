import { useState, type ReactNode } from "react";
import { Link } from "react-router";
import { useMutation, useQueryClient } from "@tanstack/react-query";
import { Edit, Trash2, TriangleAlert } from "lucide-react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { ConfirmDialog } from "@/components/ui/confirm-dialog";
import { Loader } from "@/components/ui/loader";
import { DataSourceLogo } from "@/components/data-sources/DataSourceLogo";
import { usePermissions } from "@/lib/permissions";
import { getDataSourceMeta } from "@/lib/dataSourceMeta";
import { apiErrorMessage, api } from "@/lib/api";
import { exactTs } from "@/lib/scheduling";
import type { DataSource, DataSourceType } from "@/types/api";

/** Availability (On/Off) and health (Unknown/OK/Failing) are independent axes shown as one indicator. States differ
 * by text and shape/motion, never colour alone (WCAG 1.4.1); the pulse uses `motion-safe:`. */
export function HealthIndicator({ ds }: { ds: DataSource }) {
  if (ds.paused) {
    return (
      <Health
        title={ds.pause_reason ?? "Turned off by an admin. Queries can't run against this source."}
        dot={<span className="size-2.5 rounded-full border-2 border-muted-foreground/60" />}
        label="Off"
      />
    );
  }
  if (ds.health_state === "failing") {
    return (
      <Health
        title={ds.health_error ?? "The last connection attempt failed."}
        dot={<TriangleAlert className="size-3.5 shrink-0 text-destructive" aria-hidden="true" />}
        label="On · Unreachable"
        labelClass="text-destructive"
      />
    );
  }
  if (ds.health_state === "ok") {
    return (
      <Health
        title={ds.health_checked_at ? `Last checked ${exactTs(Date.parse(ds.health_checked_at))}.` : undefined}
        dot={<span className="size-2.5 rounded-full bg-success motion-safe:animate-pulse" />}
        label="On · Healthy"
      />
    );
  }
  return (
    <Health
      title="Nothing has connected to this source yet. Run a query, or press Test connection on the data source page."
      dot={<span className="size-2.5 rounded-full bg-muted-foreground/40" />}
      label="On · Not checked yet"
    />
  );
}

type HealthProps = { title?: string; dot: ReactNode; label: string; labelClass?: string };

function Health({ title, dot, label, labelClass = "text-muted-foreground" }: HealthProps) {
  return (
    <div className="flex items-center gap-2" title={title}>
      {dot}
      <span className={`text-xs ${labelClass}`}>{label}</span>
    </div>
  );
}

/** `profiling` wins over `profile_failed`; the list payload carries no failure text. */
function ProfileChip({ ds }: { ds: DataSource }) {
  if (ds.profiling) {
    return (
      <Badge
        variant="outline"
        className="gap-1.5 text-muted-foreground"
        data-testid="ds-profile-chip"
        data-state="profiling"
        title="arcodash is updating what AI knows about this source."
      >
        <Loader size={12} />
        Profiling…
      </Badge>
    );
  }
  if (ds.profile_failed) {
    return (
      <Link to={`/data_sources/${ds.id}#ai-profile`}>
        <Badge
          variant="outline"
          className="gap-1.5 border-warning/30 bg-warning/10 text-warning-foreground"
          data-testid="ds-profile-chip"
          data-state="failed"
          title="The last AI profile build failed. Open the data source to see why and retry."
        >
          <span className="size-1.5 rounded-full bg-warning" />
          AI profile failed
        </Badge>
      </Link>
    );
  }
  return null;
}

export function DataSourceList({
  dataSources,
  types,
}: {
  dataSources: DataSource[];
  types: DataSourceType[];
}) {
  const queryClient = useQueryClient();
  const { isAdmin } = usePermissions();
  const [pendingDelete, setPendingDelete] = useState<DataSource | null>(null);
  const [deleteError, setDeleteError] = useState<string | null>(null);

  const deleteMutation = useMutation({
    mutationFn: (id: number) => api.deleteDataSource(id),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["data-sources"] });
      setPendingDelete(null);
    },
    onError: (err) => setDeleteError(apiErrorMessage(err, "Failed to delete data source")),
  });

  return (
    <>
      <div className="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-3 gap-4">
        {dataSources.map((ds) => {
          const { category } = getDataSourceMeta(ds.type);
          const typeName = types.find((t) => t.type === ds.type)?.name ?? ds.type;
          // No per-source `category`; reuse dataSourceMeta's, singularized.
          const categoryLabel = category.replace(/s$/, "");

          return (
            <Card key={ds.id} className="hover:shadow-md transition-shadow">
              <CardHeader className="pb-3">
                <div className="flex items-start justify-between">
                  <div className="flex items-center gap-3">
                    <DataSourceLogo type={ds.type} alt={typeName} />
                    <div className="flex-1 min-w-0">
                      <CardTitle className="text-base truncate">
                        <Link to={`/data_sources/${ds.id}`} className="hover:underline">
                          {ds.name}
                        </Link>
                      </CardTitle>
                      <CardDescription className="text-xs mt-1">
                        <span className="px-2 py-0.5 bg-secondary text-secondary-foreground rounded text-xs">
                          {typeName}
                        </span>
                        <span className="ml-2 text-muted-foreground">{categoryLabel}</span>
                      </CardDescription>
                    </div>
                  </div>
                </div>
              </CardHeader>
              <CardContent className="pt-0">
                <div className="space-y-3">
                  <div className="flex flex-wrap items-center gap-2">
                    <HealthIndicator ds={ds} />
                    {isAdmin && <ProfileChip ds={ds} />}
                  </div>
                  {ds.caller.can_edit && (
                    <div className="flex gap-2 pt-2 border-t">
                      <Button variant="ghost" size="sm" className="flex-1" title="Edit" asChild>
                        <Link to={`/data_sources/${ds.id}/edit`}>
                          <Edit className="w-3.5 h-3.5 mr-1" /> Edit
                        </Link>
                      </Button>
                      <Button
                        variant="ghost"
                        size="sm"
                        className="text-destructive hover:text-destructive"
                        title="Delete"
                        onClick={() => {
                          setDeleteError(null);
                          setPendingDelete(ds);
                        }}
                      >
                        <Trash2 className="w-3.5 h-3.5" />
                      </Button>
                    </div>
                  )}
                </div>
              </CardContent>
            </Card>
          );
        })}
      </div>

      <ConfirmDialog
        open={pendingDelete !== null}
        onOpenChange={(open) => {
          if (!open) {
            setPendingDelete(null);
            setDeleteError(null);
          }
        }}
        title="Delete data source?"
        description={`This will permanently delete "${pendingDelete?.name}". Data sources still used by a query can't be deleted.`}
        confirmLabel="Delete"
        destructive
        confirmPending={deleteMutation.isPending}
        onConfirm={() => {
          if (!pendingDelete) return;
          setDeleteError(null);
          deleteMutation.mutate(pendingDelete.id);
        }}
        error={deleteError}
      />
    </>
  );
}
