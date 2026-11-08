import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useNavigate, useParams } from "react-router";
import { DataSourceForm, type DataSourceFormValues } from "@/components/data-sources/DataSourceForm";
import { TestButton } from "@/components/data-sources/TestButton";
import { BackButton } from "@/components/nav/BackButton";
import { PageLoader } from "@/components/ui/loader";
import { apiErrorMessage, api } from "@/lib/api";

// Leave-blank-to-keep-current only applies to secret fields (the backend never returns secrets,
// so the form can't show a real value to overwrite); a blank non-secret field is a real "clear it".
const stripBlank = (o: Record<string, unknown>, secretFields: Set<string>) =>
  Object.fromEntries(
    Object.entries(o).filter(([k, v]) => v !== undefined && !(v === "" && secretFields.has(k))),
  );

export function DataSourceEditPage() {
  const params = useParams<{ id: string }>();
  const id = Number(params.id);
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const [error, setError] = useState<string | null>(null);

  const typesQuery = useQuery({
    queryKey: ["data-source-types"],
    queryFn: () => api.listDataSourceTypes(),
  });

  const dsQuery = useQuery({
    queryKey: ["data-source", id],
    queryFn: () => api.getDataSource(id),
    enabled: Number.isFinite(id),
  });

  const updateMutation = useMutation({
    mutationFn: (values: DataSourceFormValues) => {
      const type = typesQuery.data?.find((t) => t.type === values.type);
      const secretFields = new Set(type?.configuration_schema.secret ?? []);
      return api.updateDataSource(id, {
        name: values.name,
        options: stripBlank(values.options, secretFields),
      });
    },
    onSuccess: async () => {
      await queryClient.invalidateQueries({ queryKey: ["data-sources"] });
      await queryClient.invalidateQueries({ queryKey: ["data-source", id] });
      navigate(`/data_sources/${id}`);
    },
    onError: (err) => {
      setError(apiErrorMessage(err, "Failed to update data source"));
    },
  });

  const isLoading = typesQuery.isLoading || dsQuery.isLoading;
  const isError = typesQuery.isError || dsQuery.isError;
  const ds = dsQuery.data;
  // File-backed connectors (csv/sqlite) validate at upload time — a "Test connection" button has
  // nothing left to check.
  const hasFileField = Object.values(
    typesQuery.data?.find((t) => t.type === ds?.type)?.configuration_schema.properties ?? {},
  ).some((f) => f.type === "file");

  return (
    <div className="max-w-2xl space-y-6">
      <div className="flex items-center gap-3">
        <BackButton to={`/data_sources/${id}`} label="Back" />
        <h1 className="text-2xl font-semibold">Edit Data Source</h1>
      </div>

      {isLoading && <PageLoader />}

      {isError && (
        <p className="text-destructive">
          {apiErrorMessage(typesQuery.error ?? dsQuery.error, "Failed to load")}
        </p>
      )}

      {typesQuery.data && ds && (
        <DataSourceForm
          types={typesQuery.data}
          initialName={ds.name}
          initialType={ds.type}
          initialOptions={ds.options ?? {}}
          submitLabel="Save"
          isSubmitting={updateMutation.isPending}
          onSubmit={(values) => {
            setError(null);
            updateMutation.mutate(values);
          }}
          renderExtraActions={(values) =>
            hasFileField ? null : (
              <TestButton
                target={{ type: values.type, options: values.options }}
                disabled={!values.type}
              />
            )
          }
        />
      )}

      {error && <p className="text-sm text-destructive">{error}</p>}
    </div>
  );
}
