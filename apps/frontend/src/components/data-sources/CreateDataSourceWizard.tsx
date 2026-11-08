import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useNavigate } from "react-router";
import { Dialog, DialogContent, DialogHeader, DialogTitle } from "@/components/ui/dialog";
import { Card } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Separator } from "@/components/ui/separator";
import { DataSourceForm, type DataSourceFormValues } from "@/components/data-sources/DataSourceForm";
import { TestButton } from "@/components/data-sources/TestButton";
import { DataSourceLogo } from "@/components/data-sources/DataSourceLogo";
import {
  CATEGORY_ICONS,
  CATEGORY_ORDER,
  compareDataSourceTypes,
  getDataSourceMeta,
} from "@/lib/dataSourceMeta";
import { apiErrorMessage, api } from "@/lib/api";
import { matchesSearch } from "@/lib/utils";
import type { DataSourceType } from "@/types/api";

interface CreateDataSourceWizardProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
}

export function CreateDataSourceWizard({ open, onOpenChange }: CreateDataSourceWizardProps) {
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const [step, setStep] = useState<"pick" | "config">("pick");
  const [selectedType, setSelectedType] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [q, setQ] = useState("");

  const typesQuery = useQuery({
    queryKey: ["data-source-types"],
    queryFn: () => api.listDataSourceTypes(),
  });

  const createMutation = useMutation({
    mutationFn: (values: DataSourceFormValues) =>
      api.createDataSource({ name: values.name, type: values.type, options: values.options }),
    onSuccess: async (ds) => {
      await queryClient.invalidateQueries({ queryKey: ["data-sources"] });
      handleOpenChange(false);
      navigate(`/data_sources/${ds.id}`);
    },
    onError: (err) => setError(apiErrorMessage(err, "Failed to create data source")),
  });

  function handleOpenChange(next: boolean) {
    onOpenChange(next);
    if (!next) {
      setStep("pick");
      setSelectedType(null);
      setError(null);
      setQ("");
    }
  }

  const types = typesQuery.data ?? [];
  const knownTypes = types.filter((t) => t.name !== "Unknown");
  const selected = types.find((t) => t.type === selectedType);
  // File-backed connectors (csv/sqlite) validate at upload, so there is no Test connection.
  const hasFileField = Object.values(selected?.configuration_schema.properties ?? {}).some(
    (f) => f.type === "file",
  );

  // Case-insensitive match on display name and raw type, so an alias like "pg" finds its connector.
  const needle = q.trim().toLowerCase();
  const matchingTypes = knownTypes.filter((t) => matchesSearch(needle, t.name, t.type));

  // Grouping the sorted list keeps every category sorted too.
  const flatMatches = [...matchingTypes].sort(compareDataSourceTypes);
  const byCategory = new Map<string, DataSourceType[]>();
  for (const t of flatMatches) {
    const { category } = getDataSourceMeta(t.type);
    byCategory.set(category, [...(byCategory.get(category) ?? []), t]);
  }

  function renderTile(t: DataSourceType) {
    const selectType = () => {
      setSelectedType(t.type);
      setStep("config");
    };
    return (
      <Card
        key={t.type}
        role="button"
        tabIndex={0}
        className="cursor-pointer hover:bg-muted/50 transition-colors flex-row items-center gap-3 p-3"
        onClick={selectType}
        onKeyDown={(event) => {
          if (event.key === "Enter" || event.key === " ") {
            event.preventDefault();
            selectType();
          }
        }}
      >
        <DataSourceLogo type={t.type} alt={t.name} />
        <span className="font-medium">{t.name}</span>
      </Card>
    );
  }

  return (
    <>
      <Dialog open={open && step === "pick"} onOpenChange={handleOpenChange}>
        <DialogContent className="max-w-3xl">
          <DialogHeader>
            <DialogTitle>New Data Source</DialogTitle>
          </DialogHeader>

          {typesQuery.isLoading && <p className="text-muted-foreground">Loading data source types…</p>}

          {typesQuery.isError && (
            <p className="text-destructive">
              {apiErrorMessage(typesQuery.error, "Failed to load types")}
            </p>
          )}

          {typesQuery.data && (
            <Input
              autoFocus
              className="max-w-sm"
              placeholder="Search data source types…"
              value={q}
              onChange={(e) => setQ(e.target.value)}
              aria-label="Search data source types"
            />
          )}

          {typesQuery.data && needle && flatMatches.length === 0 && (
            <p className="text-sm text-muted-foreground">
              No data source type matches &ldquo;{q}&rdquo;.{" "}
              <button type="button" className="cursor-pointer underline" onClick={() => setQ("")}>
                Clear search
              </button>
            </p>
          )}

          {/* A search in progress collapses categories into one ranked grid. */}
          {needle ? (
            <div className="grid grid-cols-1 md:grid-cols-2 gap-3">
              {flatMatches.map(renderTile)}
            </div>
          ) : (
            <div className="space-y-6">
              {CATEGORY_ORDER.filter((cat) => byCategory.has(cat)).map((cat) => {
                const CategoryIcon = CATEGORY_ICONS[cat];
                return (
                  <div key={cat} className="space-y-3">
                    <div className="flex items-center gap-2 text-sm font-medium text-muted-foreground">
                      <CategoryIcon className="h-4 w-4" />
                      {cat}
                    </div>
                    <Separator />
                    <div className="grid grid-cols-1 md:grid-cols-2 gap-3">
                      {byCategory.get(cat)!.map(renderTile)}
                    </div>
                  </div>
                );
              })}
            </div>
          )}
        </DialogContent>
      </Dialog>

      <Dialog open={open && step === "config"} onOpenChange={handleOpenChange}>
        <DialogContent className="max-w-2xl">
          <DialogHeader>
            <DialogTitle>{selected?.name ?? "Configure data source"}</DialogTitle>
          </DialogHeader>

          {types.length > 0 && selectedType && (
            <DataSourceForm
              key={selectedType}
              types={types}
              presetType={selectedType}
              submitLabel="Create"
              isSubmitting={createMutation.isPending}
              onSubmit={(values) => {
                setError(null);
                createMutation.mutate(values);
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
        </DialogContent>
      </Dialog>
    </>
  );
}
