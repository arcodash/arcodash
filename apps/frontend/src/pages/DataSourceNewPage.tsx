import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useNavigate } from "react-router";
import { DataSourceForm, type DataSourceFormValues } from "@/components/data-sources/DataSourceForm";
import { TestButton } from "@/components/data-sources/TestButton";
import { apiErrorMessage, api } from "@/lib/api";

export function DataSourceNewPage() {
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const [error, setError] = useState<string | null>(null);

  const typesQuery = useQuery({
    queryKey: ["data-source-types"],
    queryFn: () => api.listDataSourceTypes(),
  });

  const createMutation = useMutation({
    mutationFn: (values: DataSourceFormValues) =>
      api.createDataSource({ name: values.name, type: values.type, options: values.options }),
    onSuccess: async (ds) => {
      await queryClient.invalidateQueries({ queryKey: ["data-sources"] });
      navigate(`/data_sources/${ds.id}`);
    },
    onError: (err) => {
      setError(apiErrorMessage(err, "Failed to create data source"));
    },
  });

  return (
    <div className="max-w-2xl space-y-6">
      <h1 className="text-2xl font-semibold">New Data Source</h1>

      {typesQuery.isLoading && <p className="text-muted-foreground">Loading data source types…</p>}

      {typesQuery.isError && (
        <p className="text-destructive">{apiErrorMessage(typesQuery.error, "Failed to load types")}</p>
      )}

      {typesQuery.data && (
        <DataSourceForm
          types={typesQuery.data}
          submitLabel="Create"
          isSubmitting={createMutation.isPending}
          onSubmit={(values) => {
            setError(null);
            createMutation.mutate(values);
          }}
          renderExtraActions={(values) => (
            <TestButton
              target={{ type: values.type, options: values.options }}
              disabled={!values.type}
            />
          )}
        />
      )}

      {error && <p className="text-sm text-destructive">{error}</p>}
    </div>
  );
}
