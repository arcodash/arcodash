import { useRef, useState } from "react";
import { useMutation, useQuery } from "@tanstack/react-query";
import { Link } from "react-router";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Loader } from "@/components/ui/loader";
import { api, apiErrorMessage } from "@/lib/api";
import { aiErrorMessage } from "@/lib/ai";
import { useAuth } from "@/lib/auth";
import { usePermissions } from "@/lib/permissions";
import {
  createSuggestedCheck,
  PartialCheckError,
  SuggestionCard,
  type CreateStatus,
} from "@/components/data-sources/SuggestionCard";
import type { CoveredTable, MonitorKind, MonitorSuggestion } from "@/types/api";

const MAX_TABLES_PER_RUN = 20;

/** One suggestion's local mutable state, keyed by response position (up to 3 per table, so `table`+`kind` is not
 * unique). Lives here so the batch create loop and summary read the values a user edited on a card. */
interface CardState {
  suggestion: MonitorSuggestion;
  op: string;
  value: string;
  monitorKind: MonitorKind;
  selected: boolean;
  createStatus: CreateStatus;
  testFailed: boolean;
}

interface Results {
  cards: Map<string, CardState>;
  covered: CoveredTable[];
}

function buildInitialResults(suggestions: MonitorSuggestion[], covered: CoveredTable[]): Results {
  const cards = new Map(
    suggestions.map((s, index): [string, CardState] => [
      `${s.table}::${s.kind}::${index}`,
      {
        suggestion: s,
        op: s.op,
        value: String(s.value),
        monitorKind: s.kind === "row_count" ? "baseline" : "threshold",
        selected: false,
        createStatus: { kind: "idle" },
        testFailed: false,
      },
    ]),
  );
  return { cards, covered };
}

export function DataQualityChecksCard({ dataSourceId }: { dataSourceId: number }) {
  const { clientConfig } = useAuth();
  const { has, isAdmin } = usePermissions();

  const [scopeOpen, setScopeOpen] = useState(false);
  const [tableSearch, setTableSearch] = useState("");
  const [selectedTables, setSelectedTables] = useState<Set<string>>(new Set());
  const [results, setResults] = useState<Results | null>(null);
  const [runError, setRunError] = useState<string | null>(null);
  const [lastRunTables, setLastRunTables] = useState<string[] | null>(null);
  const [batchSummary, setBatchSummary] = useState<string | null>(null);
  const [batchRunning, setBatchRunning] = useState(false);
  const abortRef = useRef<AbortController | null>(null);

  const schemaQuery = useQuery({
    queryKey: ["data-source-schema", dataSourceId],
    queryFn: () => api.getDataSourceSchema(dataSourceId, false),
  });
  const allTableNames = schemaQuery.data?.schema.tables.map((t) => t.name) ?? [];

  const suggestMutation = useMutation({
    mutationFn: (tables: string[]) => {
      const controller = new AbortController();
      abortRef.current = controller;
      return api.suggestMonitors(dataSourceId, { tables }, controller.signal);
    },
    onMutate: (tables) => {
      setRunError(null);
      setBatchSummary(null);
      setLastRunTables(tables);
    },
    onSuccess: (res) => {
      // scopeOpen closes only on "ok"; on failure it stays so the >20-table picker remains the
      // recovery path.
      if (res.status === "ok") {
        setResults(buildInitialResults(res.suggestions, res.covered));
        setScopeOpen(false);
        setSelectedTables(new Set());
        setTableSearch("");
      } else {
        setResults(null);
        setRunError(aiErrorMessage(res));
      }
    },
    onError: (err) => {
      if (err instanceof DOMException && err.name === "AbortError") return;
      setResults(null);
      setRunError(apiErrorMessage(err, "Failed to suggest checks"));
    },
  });

  function startFromSchema(freshTableNames: string[]) {
    if (freshTableNames.length <= MAX_TABLES_PER_RUN) suggestMutation.mutate(freshTableNames);
    else setScopeOpen(true);
  }

  function handlePrimaryClick() {
    if (schemaQuery.isError) {
      setRunError(apiErrorMessage(schemaQuery.error, "Failed to load schema"));
      return;
    }
    startFromSchema(allTableNames);
  }

  // A retry from a schema-load failure must re-fetch, not re-show the stale cached error via
  // handlePrimaryClick.
  async function retryAfterSchemaFailure() {
    const result = await schemaQuery.refetch();
    if (result.isError) {
      setRunError(apiErrorMessage(result.error, "Failed to load schema"));
    } else {
      setRunError(null);
      startFromSchema(result.data?.schema.tables.map((t) => t.name) ?? []);
    }
  }

  // Re-check against the live schema: a table dropped since the last run must never be re-sent; an
  // empty or unknown prior run falls back to the first-run gate.
  function handleRetry() {
    const retryTables = (lastRunTables ?? []).filter((t) => allTableNames.includes(t));
    if (retryTables.length > 0) suggestMutation.mutate(retryTables);
    else if (schemaQuery.isError) void retryAfterSchemaFailure();
    else handlePrimaryClick();
  }

  function toggleTable(name: string) {
    setSelectedTables((prev) => {
      const next = new Set(prev);
      if (next.has(name)) {
        next.delete(name);
      } else if (next.size < MAX_TABLES_PER_RUN) {
        next.add(name);
      }
      return next;
    });
  }

  function editCards(edit: (cards: Map<string, CardState>) => void) {
    setResults((prev) => {
      if (!prev) return prev;
      const cards = new Map(prev.cards);
      edit(cards);
      return { ...prev, cards };
    });
  }

  function updateCard(key: string, patch: Partial<CardState>) {
    editCards((cards) => {
      const existing = cards.get(key);
      if (existing) cards.set(key, { ...existing, ...patch });
    });
  }

  async function runBatch() {
    if (!results) return;
    const selectedKeys = Array.from(results.cards.entries())
      .filter(([, c]) => c.selected && c.createStatus.kind !== "created" && !c.testFailed)
      .map(([key]) => key);
    if (selectedKeys.length === 0) return;

    setBatchRunning(true);
    setBatchSummary(null);
    let succeeded = 0;
    for (const key of selectedKeys) {
      const card = results.cards.get(key);
      if (!card) continue;
      const numericValue = Number(card.value);
      if (card.monitorKind === "threshold" && (card.value.trim() === "" || !Number.isFinite(numericValue))) {
        updateCard(key, { createStatus: { kind: "failed", error: "Enter a number" } });
        continue;
      }
      const resume = card.createStatus.kind === "failed" ? (card.createStatus.partial ?? {}) : {};
      updateCard(key, { createStatus: { kind: "creating" } });
      try {
        const { monitorId, subscribed } = await createSuggestedCheck(
          dataSourceId,
          card.suggestion,
          card.op,
          numericValue,
          resume,
          card.monitorKind,
        );
        updateCard(key, { createStatus: { kind: "created", monitorId, subscribed }, selected: false });
        succeeded += 1;
      } catch (err) {
        updateCard(key, {
          createStatus:
            err instanceof PartialCheckError
              ? { kind: "failed", error: err.message, partial: err.partial }
              : { kind: "failed", error: apiErrorMessage(err, "Failed to create check") },
        });
      }
    }
    setBatchRunning(false);
    const failedSuffix =
      succeeded < selectedKeys.length ? " Retry the one that failed, or dismiss it." : "";
    setBatchSummary(`${succeeded} of ${selectedKeys.length} checks created.${failedSuffix}`);
  }

  if (!clientConfig?.ai_monitor_suggestions || !has("create_query") || !has("create_monitor")) {
    return null;
  }

  const providerConfigured = clientConfig.ai_sql_generation;

  const cardList = results ? [...results.cards.values()] : [];
  const createdMonitorIds = cardList.flatMap((c) =>
    c.createStatus.kind === "created" ? [c.createStatus.monitorId] : [],
  );
  const createdCount = createdMonitorIds.length;
  const selectedCount = cardList.filter((c) => c.selected && c.createStatus.kind !== "created").length;

  const visibleTableOrder = allTableNames.filter(
    (name) =>
      cardList.some((c) => c.suggestion.table === name) || !!results?.covered.some((c) => c.table === name),
  );

  return (
    <div className="rounded-md border border-border p-4 space-y-4">
      <div className="flex items-center justify-between">
        <h2 className="text-sm font-medium">Data quality checks</h2>
      </div>

      {!providerConfigured ? (
        isAdmin ? (
          <p className="text-sm text-muted-foreground">
            AI isn't set up for your organization yet.{" "}
            <Link to="/organization/ai" className="text-primary underline">
              Add an AI provider
            </Link>{" "}
            to get check suggestions.
          </p>
        ) : (
          <p className="text-sm text-muted-foreground">
            AI isn't set up for your organization yet. Ask an admin to add an AI provider.
          </p>
        )
      ) : (
        <>
          {createdCount > 0 && (
            <div className="rounded-md border border-border bg-muted/40 p-3 text-sm">
              <p>
                <span className="font-medium">
                  {createdCount} {createdCount === 1 ? "check" : "checks"} created.
                </span>{" "}
                They run every 15 minutes and email you when they fire. Adjust thresholds any time
                on the monitor page.{" "}
                <Link
                  to={createdCount === 1 ? `/monitors/${createdMonitorIds[0]}` : "/monitors"}
                  className="text-primary underline"
                >
                  Add teammates
                </Link>
              </p>
            </div>
          )}

          {results == null && !suggestMutation.isPending && !scopeOpen && (
            <div className="space-y-2">
              <p className="text-sm text-muted-foreground">
                Let AI read this data source's schema and suggest checks — stale tables, unexpected
                nulls, duplicate IDs.
              </p>
              <Button
                onClick={handlePrimaryClick}
                disabled={schemaQuery.isLoading}
                title={schemaQuery.isError ? "Couldn't load this data source's schema" : undefined}
              >
                Suggest checks with AI
              </Button>
              <p className="text-xs text-muted-foreground">
                Only table and column names and types are sent to your AI provider. Never your data.
              </p>
            </div>
          )}

          {scopeOpen && !suggestMutation.isPending && (
            <div className="space-y-2">
              <p className="text-sm text-muted-foreground">
                This data source has {allTableNames.length} tables. Pick up to {MAX_TABLES_PER_RUN} to
                look at.
              </p>
              <Input
                placeholder="Search tables…"
                value={tableSearch}
                onChange={(e) => setTableSearch(e.target.value)}
              />
              <div className="max-h-60 space-y-1 overflow-y-auto rounded-md border border-border p-2">
                {allTableNames
                  .filter((name) => name.toLowerCase().includes(tableSearch.toLowerCase()))
                  .map((name) => (
                    <label key={name} className="flex items-center gap-2 text-sm">
                      <input
                        type="checkbox"
                        className="size-3.5 cursor-pointer accent-primary"
                        checked={selectedTables.has(name)}
                        disabled={!selectedTables.has(name) && selectedTables.size >= MAX_TABLES_PER_RUN}
                        onChange={() => toggleTable(name)}
                      />
                      {name}
                    </label>
                  ))}
              </div>
              <Button
                onClick={() => suggestMutation.mutate(Array.from(selectedTables))}
                disabled={selectedTables.size === 0 || selectedTables.size > MAX_TABLES_PER_RUN}
              >
                Suggest checks for {selectedTables.size} tables
              </Button>
            </div>
          )}

          {suggestMutation.isPending && (
            <div className="flex items-center gap-2 text-sm text-muted-foreground">
              <Loader size={16} />
              Looking at your schema…
              <Button size="sm" variant="outline" onClick={() => abortRef.current?.abort()}>
                Cancel
              </Button>
            </div>
          )}

          {runError && (
            <div className="flex items-center justify-between gap-3 rounded-md border border-destructive/30 bg-destructive/10 px-3 py-2 text-sm text-destructive">
              <span>{runError}</span>
              <Button
                variant="outline"
                size="sm"
                onClick={handleRetry}
                disabled={suggestMutation.isPending}
              >
                Try again
              </Button>
            </div>
          )}

          {results && (
            <div className="space-y-4">
              {visibleTableOrder.length === 0 ? (
                <div className="space-y-2">
                  <p className="text-sm text-muted-foreground">Nothing left here.</p>
                  <Button
                    size="sm"
                    variant="outline"
                    onClick={() => {
                      setResults(null);
                      setRunError(null);
                    }}
                  >
                    Try again
                  </Button>
                </div>
              ) : (
                <>
                  <p className="text-sm text-muted-foreground">
                    {results.cards.size} checks suggested across{" "}
                    {visibleTableOrder.length} tables. Test one before creating it — AI is guessing
                    thresholds from your schema alone. You can change any threshold later.
                  </p>
                  {batchSummary && <p className="text-sm text-muted-foreground">{batchSummary}</p>}

                  {visibleTableOrder.map((table) => {
                    const cardsForTable = Array.from(results.cards.entries()).filter(
                      ([, c]) => c.suggestion.table === table,
                    );
                    const coveredEntry = results.covered.find((c) => c.table === table);
                    return (
                      <div key={table} className="space-y-2">
                        <p className="text-xs font-medium uppercase tracking-wide text-muted-foreground">
                          {table}
                        </p>
                        {coveredEntry && (
                          <div className="rounded-md border border-dashed border-border bg-muted/30 p-3 text-sm text-muted-foreground">
                            Already covered by an existing check. →{" "}
                            <Link
                              to={`/monitors/${coveredEntry.monitor_id}`}
                              className="text-primary underline"
                            >
                              View monitor
                            </Link>
                          </div>
                        )}
                        {cardsForTable.map(([key, card]) => (
                          <SuggestionCard
                            key={key}
                            dataSourceId={dataSourceId}
                            suggestion={card.suggestion}
                            op={card.op}
                            value={card.value}
                            monitorKind={card.monitorKind}
                            onMonitorKindChange={(monitorKind) => updateCard(key, { monitorKind })}
                            onOpChange={(op) => updateCard(key, { op })}
                            onValueChange={(value) => updateCard(key, { value })}
                            selected={card.selected}
                            onSelectedChange={(selected) => updateCard(key, { selected })}
                            createStatus={card.createStatus}
                            onCreateStatusChange={(createStatus) => updateCard(key, { createStatus })}
                            onTestFailedChange={(testFailed) =>
                              updateCard(key, { testFailed, ...(testFailed ? { selected: false } : {}) })
                            }
                            onDismiss={() => editCards((cards) => cards.delete(key))}
                          />
                        ))}
                      </div>
                    );
                  })}
                </>
              )}
            </div>
          )}
        </>
      )}

      {selectedCount > 0 && (
        <div className="flex items-center justify-between gap-3 rounded-md border border-border bg-card p-3 shadow-md">
          <div className="flex items-center gap-2">
            <Button
              size="sm"
              variant="ghost"
              onClick={() =>
                editCards((cards) => {
                  for (const [key, c] of cards) {
                    if (c.createStatus.kind !== "created" && !c.testFailed) cards.set(key, { ...c, selected: true });
                  }
                })
              }
            >
              Select all
            </Button>
            <Button
              size="sm"
              variant="ghost"
              onClick={() =>
                editCards((cards) => {
                  for (const [key, c] of cards) cards.set(key, { ...c, selected: false });
                })
              }
            >
              Clear
            </Button>
          </div>
          <Button size="sm" onClick={runBatch} disabled={batchRunning}>
            {batchRunning ? "Creating…" : `Create ${selectedCount} checks`}
          </Button>
        </div>
      )}
    </div>
  );
}
