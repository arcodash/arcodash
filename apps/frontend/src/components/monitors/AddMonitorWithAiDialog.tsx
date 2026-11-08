import { useEffect, useRef, useState } from "react";
import { Link, useNavigate } from "react-router";
import { useQuery } from "@tanstack/react-query";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { AiSettingsLink, AskTimeline } from "@/components/ai/AskTimeline";
import { useAskRun } from "@/components/ai/askRun";
import { api } from "@/lib/api";
import { usePermissions } from "@/lib/permissions";

const MAX_PROMPT_CHARS = 4000;
const SOURCE = "monitors_page";

interface Props {
  open: boolean;
  onOpenChange: (open: boolean) => void;
}

export function AddMonitorWithAiDialog({ open, onOpenChange }: Props) {
  const navigate = useNavigate();
  const { has } = usePermissions();
  const isAdmin = has("admin");
  const [prompt, setPrompt] = useState("");
  const [pickedSource, setPickedSource] = useState<number | null>(null);

  const sourcesQuery = useQuery({ queryKey: ["data-sources"], queryFn: () => api.listDataSources(), enabled: open });
  const sources = sourcesQuery.data?.data_sources;
  const sourceId = pickedSource ?? sources?.[0]?.id ?? null;

  const { state, start, cancel, reset } = useAskRun(api.askMonitorStream);
  const { phase, runId, failReason, suggested, preview } = state;
  const running = phase === "running";
  const failed = phase === "failed";

  const reportedRef = useRef(0);
  useEffect(() => {
    if (reportedRef.current === runId) return;
    if (phase === "done" && suggested) {
      reportedRef.current = runId;
      onOpenChange(false);
      navigate("/monitors/new", {
        state: {
          aiDraft: {
            sql: suggested.sql,
            data_source_id: suggested.data_source_id,
            query_name: prompt.trim().slice(0, 80),
            suggestion: suggested.suggestion,
            columns: preview?.columns,
          },
        },
      });
    }
  }, [phase, runId, suggested, preview, prompt, navigate, onOpenChange]);

  const budget = failed && (failReason === "ai_budget_exceeded" || failReason === "budget_exceeded");
  const forbidden = failed && failReason === "forbidden";
  const canSubmit = prompt.trim() !== "" && sourceId != null && !running;

  function submit() {
    if (!canSubmit || sourceId == null) return;
    start({ data_source_id: sourceId, prompt: prompt.trim() }, SOURCE);
  }

  function openInEditor() {
    onOpenChange(false);
    navigate("/queries/new", { state: { askQuestion: prompt, dataSourceId: sourceId } });
  }

  const noSources = sources != null && sources.length === 0;
  const started = phase !== "idle";

  return (
    <Dialog open={open} onOpenChange={(o) => !running && onOpenChange(o)}>
      <DialogContent
        className="sm:max-w-xl"
        data-testid="monitor-ask-dialog"
        onEscapeKeyDown={(e) => running && e.preventDefault()}
        onInteractOutside={(e) => running && e.preventDefault()}
      >
        <DialogHeader>
          <DialogTitle>Add a monitor with AI</DialogTitle>
          <DialogDescription>
            Describe what to watch. AI writes the query, runs it, and drafts the monitor — you review it before
            anything is saved.
          </DialogDescription>
        </DialogHeader>

        {noSources ? (
          <p className="text-sm text-muted-foreground">
            Add a data source first — AI needs its schema to write SQL.{" "}
            {isAdmin && (
              <Link to="/data_sources/new" className="text-primary underline">
                Add a data source
              </Link>
            )}
          </p>
        ) : (
          <div className="space-y-3">
            {sources && sources.length > 1 && (
              <div className="space-y-1.5">
                <Label>Data source</Label>
                <Select
                  value={sourceId != null ? String(sourceId) : ""}
                  disabled={running}
                  onValueChange={(v) => {
                    setPickedSource(Number(v));
                    reset();
                  }}
                >
                  <SelectTrigger className="w-full" aria-label="Data source" data-testid="monitor-ask-source">
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
              </div>
            )}
            <Input
              aria-label="Describe what to monitor"
              data-testid="monitor-ask-prompt"
              placeholder="e.g. alert me when daily signups drop"
              maxLength={MAX_PROMPT_CHARS}
              value={prompt}
              disabled={running}
              autoFocus
              onChange={(e) => setPrompt(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter" && !e.nativeEvent.isComposing) {
                  e.preventDefault();
                  submit();
                }
              }}
            />
            {!started && (
              <p className="text-xs text-muted-foreground" data-testid="monitor-ai-disclosure">
                AI sees your schema and, unless an admin turned off sample values in{" "}
                <AiSettingsLink isAdmin={isAdmin} />, a few sample values per column and the first rows of the
                query it writes.
              </p>
            )}
            <AskTimeline
              state={budget ? { ...state, error: null } : state}
              onCancel={cancel}
              failedTitle="Couldn't draft a monitor"
            />
            {budget && (
              <p
                role="alert"
                className="rounded-md border border-warning/30 bg-warning/10 px-3 py-2 text-sm text-warning-foreground"
              >
                Your organization has reached its monthly AI budget.{" "}
                {isAdmin && (
                  <Link to="/organization/ai" className="underline">
                    Raise it in AI Settings
                  </Link>
                )}
              </p>
            )}
          </div>
        )}

        {!running && !noSources && (
          <DialogFooter>
            {failed ? (
              <>
                <Button variant="outline" data-testid="monitor-ask-open-editor" onClick={openInEditor}>
                  Open in editor
                </Button>
                {!budget && !forbidden && (
                  <Button disabled={!canSubmit} onClick={submit}>
                    Try again
                  </Button>
                )}
              </>
            ) : (
              <>
                <Button variant="outline" onClick={() => onOpenChange(false)}>
                  Cancel
                </Button>
                <Button data-testid="monitor-ask-submit" disabled={!canSubmit} onClick={submit}>
                  Draft monitor
                </Button>
              </>
            )}
          </DialogFooter>
        )}
      </DialogContent>
    </Dialog>
  );
}
