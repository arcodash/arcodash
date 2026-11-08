import { useState } from "react";
import { Link } from "react-router";
import { ExternalLink } from "lucide-react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogFooter,
} from "@/components/ui/dialog";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { getViz } from "@/components/visualizations/registry";
import { StateBadge } from "@/components/common/StateBadge";
import { api, apiErrorMessage } from "@/lib/api";
import { matchesSearch } from "@/lib/utils";
import type { Widget } from "@/types/api";

function vizOptionLabel(v: { name: string; type: string }): string {
  return `${v.name.replace(/^✨\s*/, "")} · ${getViz(v.type)?.name ?? v.type}`;
}

/** Worst-first, so the monitors most likely to need attention sort to the top of the picker. */
const SEVERITY_ORDER: Record<string, number> = { triggered: 0, unevaluable: 1, unknown: 2, ok: 3 };
function bySeverity(a: { name: string; state: string }, b: { name: string; state: string }): number {
  const diff = (SEVERITY_ORDER[a.state] ?? 2) - (SEVERITY_ORDER[b.state] ?? 2);
  return diff !== 0 ? diff : a.name.localeCompare(b.name);
}

type Mode = "viz" | "text" | "monitor";
const MODES: [Mode, string][] = [
  ["viz", "Visualization"],
  ["text", "Text"],
  ["monitor", "Monitor"],
];

function LoadError({ query, fallback }: { query: { error: unknown; refetch: () => unknown }; fallback: string }) {
  return (
    <div className="flex items-center justify-between gap-2 px-2 py-1.5">
      <p className="text-sm text-destructive">{apiErrorMessage(query.error, fallback)}</p>
      <Button size="sm" variant="outline" onClick={() => query.refetch()}>
        Retry
      </Button>
    </div>
  );
}

interface Props {
  dashboardId: number;
  open: boolean;
  onClose: () => void;
  widgets?: Widget[];
}

export function AddWidgetDialog({ dashboardId, open, onClose, widgets = [] }: Props) {
  const qc = useQueryClient();
  const [mode, setMode] = useState<Mode>("viz");
  const [queryFilter, setQueryFilter] = useState("");
  const [queryId, setQueryId] = useState<number | null>(null);
  const [vizId, setVizId] = useState<number | null>(null);
  const [text, setText] = useState("");
  const [monitorFilter, setMonitorFilter] = useState("");
  const [selectedIds, setSelectedIds] = useState<number[]>([]);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const queriesQ = useQuery({
    queryKey: ["queries"],
    queryFn: () => api.listQueries(),
    enabled: open && mode === "viz",
  });
  // A draft query is visible only to its owner and sharees; a widget on it would be blank for
  // everyone else once published.
  const publishableQueries = (queriesQ.data ?? []).filter((q) => q.published);
  const filteredQueries = publishableQueries.filter((q) => matchesSearch(queryFilter, q.name || `Query #${q.id}`));

  const vizsQ = useQuery({
    queryKey: ["visualizations", queryId],
    queryFn: () => api.listVisualizations(queryId!),
    enabled: queryId != null,
  });
  const noVizs = vizsQ.isSuccess && vizsQ.data.length === 0;

  const monitorsQ = useQuery({
    queryKey: ["monitors"],
    queryFn: () => api.listMonitors(),
    enabled: open && mode === "monitor",
  });
  const sortedMonitors = [...(monitorsQ.data ?? [])].sort(bySeverity);
  const filteredMonitors = sortedMonitors.filter((m) => matchesSearch(monitorFilter, m.name));

  function toggleMonitor(id: number) {
    setSelectedIds((ids) => (ids.includes(id) ? ids.filter((x) => x !== id) : [...ids, id]));
  }

  async function handleAdd() {
    setSaving(true);
    setError(null);
    try {
      const row = Math.max(
        0,
        ...widgets.map((w) => (w.options?.position?.row ?? 0) + (w.options?.position?.sizeY ?? 2)),
      );
      const options =
        mode === "monitor"
          ? {
              position: {
                col: 0,
                row,
                sizeX: 2,
                sizeY: selectedIds.length > 1 ? 3 : 2,
              },
              monitors: selectedIds,
            }
          : { position: { col: 0, row, sizeX: 3, sizeY: 2 } };
      await api.createWidget({
        dashboard_id: dashboardId,
        visualization_id: mode === "viz" ? vizId : null,
        text: mode === "text" ? text : null,
        options,
      });
      await qc.invalidateQueries({ queryKey: ["dashboard"] });
      handleClose();
    } catch (e) {
      setError(apiErrorMessage(e, "Failed to add widget"));
    } finally {
      setSaving(false);
    }
  }

  function handleClose() {
    setQueryFilter("");
    setQueryId(null);
    setVizId(null);
    setText("");
    setMonitorFilter("");
    setSelectedIds([]);
    setMode("viz");
    setError(null);
    onClose();
  }

  const canAdd =
    mode === "text"
      ? text.trim().length > 0
      : mode === "monitor"
        ? selectedIds.length > 0
        : vizId != null;
  const addBlockedReason =
    mode === "text"
      ? "Enter some text or markdown"
      : mode === "monitor"
        ? "Select at least one monitor"
        : queryId == null
          ? "Select a query"
          : noVizs
            ? "This query has no visualizations yet"
            : "Select a visualization";

  return (
    <Dialog open={open} onOpenChange={(o) => !o && handleClose()}>
      <DialogContent className="max-w-md">
        <DialogHeader>
          <DialogTitle>Add Widget</DialogTitle>
        </DialogHeader>

        <div className="space-y-4">
          <div className="flex gap-2">
            {MODES.map(([m, label]) => (
              <Button key={m} variant={mode === m ? "default" : "outline"} size="sm" onClick={() => setMode(m)}>
                {label}
              </Button>
            ))}
          </div>

          {mode === "viz" && (
            <>
              <div className="space-y-1">
                <Label htmlFor="add-widget-query">
                  Query<span className="text-destructive"> *</span>
                </Label>
                <Input
                  placeholder="Search queries…"
                  value={queryFilter}
                  onChange={(e) => setQueryFilter(e.target.value)}
                  data-testid="add-widget-query-filter"
                />
                <Select
                  value={queryId != null ? String(queryId) : ""}
                  onValueChange={(v) => {
                    setQueryId(Number(v));
                    setVizId(null);
                  }}
                >
                  <SelectTrigger className="w-full" id="add-widget-query" data-testid="add-widget-query-select">
                    <SelectValue placeholder="Select a query…" />
                  </SelectTrigger>
                  <SelectContent>
                    {queriesQ.isPending ? (
                      <p className="px-2 py-1.5 text-sm text-muted-foreground">Loading queries…</p>
                    ) : queriesQ.isError ? (
                      <LoadError query={queriesQ} fallback="Failed to load queries" />
                    ) : filteredQueries.length === 0 ? (
                      <p className="px-2 py-1.5 text-sm text-muted-foreground">No queries match</p>
                    ) : (
                      filteredQueries.map((q) => (
                        <SelectItem key={q.id} value={String(q.id)}>
                          {q.name || `Query #${q.id}`}
                        </SelectItem>
                      ))
                    )}
                  </SelectContent>
                </Select>
                <p className="text-xs text-muted-foreground">
                  Only published queries are shown — a draft's results aren't visible to anyone
                  else the dashboard is shared with.
                </p>
              </div>

              {queryId && (
                <div className="space-y-1">
                  <Label htmlFor="add-widget-viz">
                    Visualization<span className="text-destructive"> *</span>
                  </Label>
                  <Select
                    value={vizId != null ? String(vizId) : ""}
                    onValueChange={(v) => setVizId(Number(v))}
                  >
                    <SelectTrigger
                      className="w-full"
                      id="add-widget-viz"
                      data-testid="add-widget-viz-select"
                      disabled={noVizs}
                    >
                      <SelectValue placeholder={noVizs ? "No visualizations yet" : "Select a visualization…"} />
                    </SelectTrigger>
                    <SelectContent>
                      {vizsQ.data?.map((v) => (
                        <SelectItem key={v.id} value={String(v.id)}>
                          {vizOptionLabel(v)}
                        </SelectItem>
                      ))}
                    </SelectContent>
                  </Select>
                  {noVizs && (
                    <p className="text-xs text-muted-foreground">
                      This query has no visualizations yet —{" "}
                      <Link
                        to={`/queries/${queryId}`}
                        target="_blank"
                        rel="noreferrer"
                        className="inline-flex items-center gap-1 underline underline-offset-2 hover:text-foreground"
                      >
                        add one in the query editor
                        <ExternalLink className="size-3" aria-hidden="true" />
                      </Link>
                      , then reselect the query here.
                    </p>
                  )}
                </div>
              )}
            </>
          )}

          {mode === "text" && (
            <div className="space-y-1">
              <Label>
                Content<span className="text-destructive"> *</span>
              </Label>
              <textarea
                className="w-full rounded-md border border-input bg-background px-3 py-2 text-sm"
                rows={5}
                value={text}
                onChange={(e) => setText(e.target.value)}
                placeholder="Enter text or markdown…"
              />
            </div>
          )}

          {mode === "monitor" && (
            <div className="space-y-1">
              <Label>
                Monitors<span className="text-destructive"> *</span>
              </Label>
              <Input
                placeholder="Search monitors…"
                value={monitorFilter}
                onChange={(e) => setMonitorFilter(e.target.value)}
                data-testid="add-widget-monitor-filter"
              />
              <div className="max-h-56 space-y-0 overflow-y-auto rounded-md border border-input">
                {monitorsQ.isPending ? (
                  <p className="px-2 py-1.5 text-sm text-muted-foreground">Loading monitors…</p>
                ) : monitorsQ.isError ? (
                  <LoadError query={monitorsQ} fallback="Failed to load monitors" />
                ) : sortedMonitors.length === 0 ? (
                  <p className="px-2 py-1.5 text-sm text-muted-foreground">
                    No monitors yet — create one to add it here.
                  </p>
                ) : filteredMonitors.length === 0 ? (
                  <p className="px-2 py-1.5 text-sm text-muted-foreground">No monitors match</p>
                ) : (
                  filteredMonitors.map((m) => (
                    <label
                      key={m.id}
                      className="flex cursor-pointer items-center gap-2 px-2 py-1.5 text-sm hover:bg-muted/50"
                    >
                      <input
                        type="checkbox"
                        className="size-4 shrink-0 accent-primary"
                        checked={selectedIds.includes(m.id)}
                        onChange={() => toggleMonitor(m.id)}
                      />
                      <span className="min-w-0 flex-1 truncate">{m.name}</span>
                      <StateBadge state={m.state} />
                    </label>
                  ))
                )}
              </div>
              <p className="text-xs text-muted-foreground">
                Pick one for a single status card, or several for a compact list.
              </p>
            </div>
          )}
        </div>

        {error && <p className="text-sm text-destructive">{error}</p>}

        <DialogFooter>
          <Button variant="outline" onClick={handleClose}>
            Cancel
          </Button>
          <Button
            disabled={!canAdd || saving}
            title={!canAdd ? addBlockedReason : undefined}
            onClick={handleAdd}
          >
            {saving ? "Adding…" : "Add"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
