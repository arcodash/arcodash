import { useCallback, useEffect, useRef, useState } from "react";
import { Link, useNavigate } from "react-router";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { X } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { AskDisclosure, AskTimeline } from "@/components/ai/AskTimeline";
import { useAskRun } from "@/components/ai/askRun";
import { api } from "@/lib/api";
import { usePermissions } from "@/lib/permissions";
import type { AskEvent, Widget } from "@/types/api";

export type WidgetCreated = Extract<AskEvent, { type: "created" }>;

const MAX_PROMPT_CHARS = 4000;

/** The source most widgets query; a tie goes to the earliest widget. Falls back to the first
 * source. */
export function pickDefaultSourceId(widgets: Widget[], sourceIds: number[]): number | null {
  const counts = new Map<number, number>();
  for (const w of widgets) {
    const id = w.visualization?.query?.data_source_id;
    if (id != null && sourceIds.includes(id)) counts.set(id, (counts.get(id) ?? 0) + 1);
  }
  let best: number | null = null;
  let bestCount = 0;
  for (const [id, n] of counts) {
    if (n > bestCount) {
      best = id;
      bestCount = n;
    }
  }
  return best ?? sourceIds[0] ?? null;
}

interface Props {
  dashboardId: number;
  widgets: Widget[];
  onClose: () => void;
  onCreated: (created: WidgetCreated) => void;
}

export function AddWithAiBar({ dashboardId, widgets, onClose, onCreated }: Props) {
  const navigate = useNavigate();
  const qc = useQueryClient();
  const { has } = usePermissions();
  const [prompt, setPrompt] = useState("");
  const [pickedSource, setPickedSource] = useState<number | null>(null);
  const inputRef = useRef<HTMLInputElement>(null);

  const sourcesQuery = useQuery({ queryKey: ["data-sources"], queryFn: () => api.listDataSources() });
  const sources = sourcesQuery.data?.data_sources;
  const sourceId =
    pickedSource ?? pickDefaultSourceId(widgets, (sources ?? []).map((s) => s.id));

  const streamFn = useCallback(
    (
      body: { data_source_id: number; prompt: string },
      onEvent: (e: AskEvent) => void,
      signal?: AbortSignal,
    ) => api.askWidgetStream(dashboardId, body, onEvent, signal),
    [dashboardId],
  );
  const { state, start, cancel, reset } = useAskRun(streamFn);
  const { phase, runId, failReason, created } = state;
  const running = phase === "running";

  useEffect(() => inputRef.current?.focus(), []);

  // A tile the server finished creating after the stream was cut must still appear.
  const stateRef = useRef(state);
  stateRef.current = state;
  const lateRefetchRef = useRef<number | null>(null);
  const settleIfCut = useCallback(() => {
    const s = stateRef.current;
    if (s.phase !== "done" && s.steps.some((st) => st.kind === "visualizing")) {
      void qc.invalidateQueries({ queryKey: ["dashboard"] });
      // The server may still be inserting the widget when the first refetch lands.
      if (lateRefetchRef.current == null) {
        lateRefetchRef.current = window.setTimeout(() => {
          lateRefetchRef.current = null;
          void qc.invalidateQueries({ queryKey: ["dashboard"] });
        }, 2000);
      }
    }
  }, [qc]);
  useEffect(() => settleIfCut, [settleIfCut]);
  useEffect(
    () => () => {
      if (lateRefetchRef.current != null) window.clearTimeout(lateRefetchRef.current);
    },
    [],
  );

  const reportedRef = useRef(0);
  useEffect(() => {
    if (reportedRef.current === runId) return;
    if (phase === "done" && created) {
      reportedRef.current = runId;
      onCreated(created);
    } else if (phase === "cancelled") {
      reportedRef.current = runId;
      settleIfCut();
    }
  }, [phase, runId, created, onCreated, settleIfCut]);

  const failed = phase === "failed";
  const budget = failed && (failReason === "ai_budget_exceeded" || failReason === "budget_exceeded");
  const forbidden = failed && failReason === "forbidden";
  const canSubmit = prompt.trim() !== "" && sourceId != null && !running;

  function submit() {
    if (!canSubmit || sourceId == null) return;
    start({ data_source_id: sourceId, prompt: prompt.trim() }, "dashboard");
  }

  function openInEditor() {
    navigate("/queries/new", { state: { askQuestion: prompt, dataSourceId: sourceId } });
  }

  const noSources = sources != null && sources.length === 0;

  return (
    <div
      id="ai-widget-bar"
      role="region"
      aria-label="Add a widget with AI"
      className="space-y-2 rounded-md border border-border bg-card p-3"
      onKeyDown={(e) => {
        if (e.key === "Escape" && !running) {
          e.stopPropagation();
          onClose();
        }
      }}
    >
      {noSources ? (
        <div className="flex items-center justify-between gap-2 text-sm text-muted-foreground">
          <span>
            Add a data source first — AI needs its schema to write SQL.{" "}
            {has("admin") && (
              <Link to="/data_sources/new" className="text-primary underline">
                Add a data source
              </Link>
            )}
          </span>
          <Button variant="ghost" size="icon-sm" aria-label="Close" onClick={onClose}>
            <X />
          </Button>
        </div>
      ) : (
        <div className="flex flex-wrap items-center gap-2">
          {sources && sources.length > 1 && (
            <Select
              value={sourceId != null ? String(sourceId) : ""}
              disabled={running}
              onValueChange={(v) => {
                setPickedSource(Number(v));
                reset();
              }}
            >
              <SelectTrigger className="w-full sm:w-48" aria-label="Data source">
                <SelectValue placeholder="Data source" />
              </SelectTrigger>
              <SelectContent>
                {sources.map((ds) => (
                  <SelectItem key={ds.id} value={String(ds.id)}>
                    {ds.name}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          )}
          <Input
            ref={inputRef}
            className="min-w-0 flex-1"
            aria-label="Describe the widget you want"
            placeholder="Describe a chart, e.g. signups by country, last 30 days"
            maxLength={MAX_PROMPT_CHARS}
            value={prompt}
            disabled={running}
            onChange={(e) => setPrompt(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter" && !e.nativeEvent.isComposing) {
                e.preventDefault();
                submit();
              }
            }}
          />
          {!running && !forbidden && !budget && (
            <Button size="sm" disabled={!canSubmit} onClick={submit}>
              {failed || phase === "cancelled" ? "Try again" : "Add"}
            </Button>
          )}
          <Button
            variant="ghost"
            size="icon-sm"
            aria-label="Close"
            title={running ? "Cancel the run first" : undefined}
            disabled={running}
            onClick={onClose}
          >
            <X />
          </Button>
        </div>
      )}
      {phase === "idle" && !noSources && <AskDisclosure isAdmin={has("admin")} />}
      <AskTimeline state={budget ? { ...state, error: null } : state} onCancel={cancel} failedTitle="Couldn't add a widget" />
      {budget && (
        <p
          role="alert"
          className="rounded-md border border-warning/30 bg-warning/10 px-3 py-2 text-sm text-warning-foreground"
        >
          Your organization has reached its monthly AI budget.{" "}
          {has("admin") && (
            <Link to="/organization/ai" className="underline">
              Raise it in AI Settings
            </Link>
          )}
        </p>
      )}
      {failed && !budget && (
        <div>
          <Button variant="outline" size="sm" onClick={openInEditor}>
            Open in editor
          </Button>
        </div>
      )}
    </div>
  );
}
