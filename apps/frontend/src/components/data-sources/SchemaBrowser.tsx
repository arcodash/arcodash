import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { ChevronDown, ChevronRight, RefreshCw } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Loader } from "@/components/ui/loader";
import { apiErrorMessage, api } from "@/lib/api";
import { cn } from "@/lib/utils";

interface SchemaBrowserProps {
  dataSourceId: number | null;
  /** Optional: clicking a table/column name calls this (e.g. to insert into the editor). */
  onInsert?: (text: string) => void;
  className?: string;
}

export function SchemaBrowser({ dataSourceId, onInsert, className }: SchemaBrowserProps) {
  const [expanded, setExpanded] = useState<Set<string>>(new Set());
  const queryClient = useQueryClient();

  const schemaQuery = useQuery({
    queryKey: ["data-source-schema", dataSourceId],
    queryFn: () => api.getDataSourceSchema(dataSourceId!, false),
    enabled: dataSourceId != null,
    refetchInterval: (q) => (q.state.data?.refreshing ? 2000 : false),
  });

  const refreshMutation = useMutation({
    mutationFn: () => api.getDataSourceSchema(dataSourceId!, true),
    onSuccess: (data) => {
      queryClient.setQueryData(["data-source-schema", dataSourceId], { ...data, refreshing: true });
      void queryClient.invalidateQueries({ queryKey: ["data-source-schema", dataSourceId] });
    },
  });

  function toggle(name: string) {
    setExpanded((prev) => {
      const next = new Set(prev);
      if (next.has(name)) next.delete(name);
      else next.add(name);
      return next;
    });
  }

  const refreshing = refreshMutation.isPending || !!schemaQuery.data?.refreshing;
  const refreshLabel = refreshing ? "Refreshing schema…" : "Refresh schema";

  if (dataSourceId == null) {
    return <p className={cn("text-sm text-muted-foreground", className)}>Select a data source to browse its schema.</p>;
  }

  const tables = schemaQuery.data?.schema.tables ?? [];

  return (
    <div className={cn("flex flex-col gap-2", className)}>
      <div className="flex items-center justify-between">
        <p className="text-sm font-medium">Schema</p>
        <Button
          type="button"
          variant="ghost"
          size="icon-sm"
          onClick={() => refreshMutation.mutate()}
          disabled={refreshing}
          title={refreshLabel}
          aria-label={refreshLabel}
          data-testid="schema-refresh"
        >
          <RefreshCw className={cn("size-3.5", refreshing && "animate-spin")} />
        </Button>
      </div>

      {refreshing && tables.length > 0 && (
        <p className="text-xs text-muted-foreground" role="status" data-testid="schema-refreshing">
          Refreshing… the list updates when it's done.
        </p>
      )}

      {schemaQuery.isLoading && <Loader size={16} announce />}

      {schemaQuery.isError && (
        <p className="text-sm text-destructive">
          {apiErrorMessage(schemaQuery.error, "Failed to load schema")}
        </p>
      )}

      {refreshMutation.isError && (
        <p className="text-sm text-destructive">
          {apiErrorMessage(refreshMutation.error, "Couldn't refresh the schema")}
        </p>
      )}

      {!refreshing && schemaQuery.data?.refresh_error && (
        <p className="text-sm text-destructive" data-testid="schema-refresh-error">
          Couldn't refresh the schema: {schemaQuery.data.refresh_error}
        </p>
      )}

      {!schemaQuery.isLoading && tables.length === 0 && !schemaQuery.isError && (
        refreshing ? (
          <p
            className="flex items-center gap-2 text-sm text-muted-foreground"
            role="status"
            data-testid="schema-first-load"
          >
            <Loader size={14} /> Loading tables… This can take a minute on large databases.
          </p>
        ) : (
          <p className="text-sm text-muted-foreground">No tables found.</p>
        )
      )}

      <ul className="space-y-0.5 overflow-auto">
        {tables.map((table) => {
          const isOpen = expanded.has(table.name);
          return (
            <li key={table.name}>
              <button
                type="button"
                className="flex w-full items-center gap-1 rounded px-1 py-1 text-left text-sm hover:bg-accent cursor-pointer"
                onClick={() => toggle(table.name)}
              >
                {isOpen ? <ChevronDown className="size-3.5" /> : <ChevronRight className="size-3.5" />}
                <span
                  className="font-medium"
                  onDoubleClick={(e) => {
                    e.stopPropagation();
                    onInsert?.(table.name);
                  }}
                >
                  {table.name}
                </span>
              </button>
              {isOpen && (
                <ul className="ml-5 space-y-0.5 border-l border-border pl-2">
                  {table.columns.map((col) => (
                    <li key={col.name}>
                      <button
                        type="button"
                        className="flex w-full items-center justify-between gap-2 rounded px-1 py-0.5 text-left text-xs text-muted-foreground hover:bg-accent hover:text-foreground cursor-pointer"
                        onClick={() => onInsert?.(col.name)}
                        title={`Insert ${col.name}`}
                      >
                        <span className="truncate">{col.name}</span>
                        {col.type_name && (
                          <span className="shrink-0 text-muted-foreground/70">{col.type_name}</span>
                        )}
                      </button>
                    </li>
                  ))}
                </ul>
              )}
            </li>
          );
        })}
      </ul>
    </div>
  );
}
