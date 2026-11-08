import { useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { Database, Plus } from "lucide-react";
import { Button } from "@/components/ui/button";
import { PageHeader } from "@/components/common/PageHeader";
import { EmptyState } from "@/components/common/EmptyState";
import { DataSourceList } from "@/components/data-sources/DataSourceList";
import { CreateDataSourceWizard } from "@/components/data-sources/CreateDataSourceWizard";
import { PageLoader } from "@/components/ui/loader";
import { apiErrorMessage, api } from "@/lib/api";
import { usePermissions } from "@/lib/permissions";

/** Locked copy for the union-rule create block — matches the backend's
 * `DATA_SOURCE_CREATE_BLOCKED` 403 message verbatim so a stale-disabled-state race still shows
 * the same explanation. */
const CREATE_BLOCKED_INLINE =
  "You can't add a data source right now because your access is limited to specific data sources. Ask a workspace admin to widen your access.";
const CREATE_BLOCKED_SHORT =
  "Your access is limited to specific data sources — ask an admin to add more.";

export function DataSourcesPage() {
  const { has } = usePermissions();
  const canCreate = has("create_data_source");
  const [wizardOpen, setWizardOpen] = useState(false);
  const dataSourcesQuery = useQuery({
    queryKey: ["data-sources"],
    queryFn: () => api.listDataSources(),
    refetchInterval: (q) => (q.state.data?.data_sources.some((d) => d.profiling) ? 3000 : false),
  });
  const typesQuery = useQuery({
    queryKey: ["data-source-types"],
    queryFn: () => api.listDataSourceTypes(),
  });

  // Default true so the button doesn't flash into a disabled state before the list loads.
  const serverCanCreate = dataSourcesQuery.data?.can_create ?? true;
  const createBlocked = canCreate && !serverCanCreate;
  const dataSources = dataSourcesQuery.data?.data_sources;

  return (
    <div className="space-y-6">
      <PageHeader
        title="Data Sources"
        action={
          canCreate && (
            <Button
              onClick={() => setWizardOpen(true)}
              disabled={createBlocked}
              title={createBlocked ? CREATE_BLOCKED_SHORT : undefined}
              aria-label={createBlocked ? CREATE_BLOCKED_SHORT : undefined}
            >
              + New data source
            </Button>
          )
        }
      />

      {createBlocked && !!dataSources?.length && (
        <p className="text-sm text-muted-foreground">{CREATE_BLOCKED_INLINE}</p>
      )}

      {dataSourcesQuery.isLoading && <PageLoader />}

      {dataSourcesQuery.isError && (
        <p className="text-destructive">
          {apiErrorMessage(dataSourcesQuery.error, "Failed to load data sources")}
        </p>
      )}

      {dataSources?.length === 0 &&
        (canCreate && serverCanCreate ? (
          <EmptyState
            icon={Database}
            iconClassName="bg-sky-50 text-sky-600"
            title="Connect your first data source"
            description="Hook up a database or API, then query it and monitor its data quality."
            action={
              <Button size="sm" onClick={() => setWizardOpen(true)}>
                <Plus className="w-3 h-3" />
                Connect Your First Data Source
              </Button>
            }
          />
        ) : (
          <EmptyState
            icon={Database}
            iconClassName="bg-sky-50 text-sky-600"
            title="No data sources available"
            description="Your access is limited to specific data sources — ask a workspace admin to grant access to the ones you need."
          />
        ))}
      {!!dataSources?.length && <DataSourceList dataSources={dataSources} types={typesQuery.data ?? []} />}

      <CreateDataSourceWizard open={wizardOpen} onOpenChange={setWizardOpen} />
    </div>
  );
}
