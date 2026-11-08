import { useEffect, useRef, useState } from "react";
import { Link, useNavigate } from "react-router";
import { useMutation, useQueryClient } from "@tanstack/react-query";
import { BellRing, Sparkles } from "lucide-react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Loader } from "@/components/ui/loader";
import { Sheet, SheetContent, SheetDescription, SheetHeader, SheetTitle } from "@/components/ui/sheet";
import { EmptyState } from "@/components/common/EmptyState";
import { AiSettingsLink } from "@/components/ai/AskTimeline";
import { trimError } from "@/components/ai/askRun";
import { ApiError, api, apiErrorMessage } from "@/lib/api";
import { aiErrorMessage } from "@/lib/ai";
import { RichText } from "@/components/monitors/richText";
import { createMonitorOnQuery, suggestionOptions } from "@/lib/monitors";
import { usePermissions } from "@/lib/permissions";
import type { CoveredColumn, QueryMonitorSuggestion } from "@/types/api";

export const UNPUBLISHED_REASON =
  "Publish this query to get monitor suggestions — monitors watch the published version.";
export const UNPUBLISHED_REASON_VIEWER =
  "Monitors watch the published version of a query. Ask the owner to publish it.";
export const NO_RESULT_REASON = "Run this query once so AI can see its result columns.";

type Body =
  | { kind: "loading" }
  | { kind: "reason"; text: string }
  | { kind: "ai-error"; text: string; retry: boolean }
  | { kind: "error"; text: string }
  | { kind: "ok"; suggestions: QueryMonitorSuggestion[]; covered: CoveredColumn[]; unsupported: string | null; mode: "all" | "prompt" };

const guessedThreshold = (s: QueryMonitorSuggestion) => s.kind === "threshold" && s.confidence !== "high";

interface Added {
  id: number;
  name: string;
  subscribed: boolean;
}

export function SuggestMonitorsPane({
  queryId,
  queryName,
  hasUnpublishedChanges,
  open,
  onOpenChange,
}: {
  queryId: number;
  queryName: string;
  hasUnpublishedChanges: boolean;
  open: boolean;
  onOpenChange: (open: boolean) => void;
}) {
  const navigate = useNavigate();
  const qc = useQueryClient();
  const { has } = usePermissions();
  const [prompt, setPrompt] = useState("");
  const [body, setBody] = useState<Body>({ kind: "loading" });
  const [idx, setIdx] = useState(0);
  const [added, setAdded] = useState<Added[]>([]);
  const [announce, setAnnounce] = useState("");
  const [addError, setAddError] = useState<string | null>(null);
  const promptRef = useRef<HTMLInputElement>(null);
  const addRef = useRef<HTMLButtonElement>(null);
  const doneRef = useRef<HTMLButtonElement>(null);
  const runId = useRef(0);
  const lastPrompt = useRef("");
  const focusNext = useRef(false);

  const addMutation = useMutation({
    mutationFn: (s: QueryMonitorSuggestion) => createMonitorOnQuery(queryId, s.name, suggestionOptions(s)),
    onSettled: () => {
      void qc.invalidateQueries({ queryKey: ["monitors", "query", queryId] });
      void qc.invalidateQueries({ queryKey: ["monitors"] });
    },
  });

  async function run(p: string) {
    const id = ++runId.current;
    const text = p.trim();
    lastPrompt.current = text;
    setBody({ kind: "loading" });
    setIdx(0);
    setAdded([]);
    setAddError(null);
    let next: Body;
    try {
      const res = await api.suggestQueryMonitors(queryId, text ? { prompt: text } : {});
      next =
        res.status === "ok"
          ? {
              kind: "ok",
              suggestions: res.suggestions,
              covered: res.already_covered,
              unsupported: res.unsupported,
              mode: text ? "prompt" : "all",
            }
          : {
              kind: "ai-error",
              text: aiErrorMessage(res),
              retry: res.status === "rate_limited" || res.status === "generation_failed",
            };
    } catch (err) {
      const code = err instanceof ApiError ? err.errorCode : undefined;
      if (code === "query_unpublished") next = { kind: "reason", text: UNPUBLISHED_REASON_VIEWER };
      else if (code === "no_result") next = { kind: "reason", text: NO_RESULT_REASON };
      else if (code === "ai_budget_exceeded" || code === "budget_exceeded" || (err instanceof ApiError && err.status === 403)) {
        next = { kind: "ai-error", text: apiErrorMessage(err, "AI monitor suggestions aren't available."), retry: false };
      } else next = { kind: "error", text: apiErrorMessage(err, "Couldn't get suggestions") };
    }
    if (id === runId.current) setBody(next);
  }

  useEffect(() => {
    if (!open) {
      runId.current++;
      return;
    }
    setPrompt("");
    void run("");
    // run is recreated per render; opening is the only trigger.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open]);

  const suggestions = body.kind === "ok" ? body.suggestions : [];
  const done = body.kind === "ok" && suggestions.length > 0 && idx >= suggestions.length;
  const current = suggestions[idx];

  useEffect(() => {
    if (!focusNext.current) return;
    focusNext.current = false;
    (done ? doneRef.current : addRef.current)?.focus();
  }, [idx, done]);

  function advance() {
    focusNext.current = true;
    setAddError(null);
    setIdx((i) => i + 1);
  }

  async function onAdd(s: QueryMonitorSuggestion) {
    setAddError(null);
    try {
      const r = await addMutation.mutateAsync(s);
      setAdded((a) => [...a, { id: r.monitorId, name: s.name, subscribed: r.subscribed }]);
      setAnnounce(`Added "${s.name}".`);
      advance();
    } catch (err) {
      setAddError(apiErrorMessage(err, "Couldn't add this monitor."));
    }
  }

  function onEditFirst(s: QueryMonitorSuggestion) {
    onOpenChange(false);
    navigate(`/monitors/new?query_id=${queryId}`, { state: { aiSuggestion: { ...s, guessed: guessedThreshold(s) } } });
  }

  const loading = body.kind === "loading";
  const pending = addMutation.isPending;

  function renderCard(s: QueryMonitorSuggestion, ok: Extract<Body, { kind: "ok" }>) {
    const single = ok.mode === "prompt" && ok.suggestions.length === 1;
    return (
      <div className="space-y-3" data-testid="monitor-suggest-card">
        <div className="flex items-center justify-between gap-2">
          <span className="text-xs text-muted-foreground" data-testid="monitor-suggest-position">
            {single ? "From your description" : `${idx + 1} of ${ok.suggestions.length}`}
            {ok.covered.length > 0 && ` · ${ok.covered.length} already covered by existing monitors`}
          </span>
          <Badge variant="secondary" data-testid="monitor-suggest-kind">
            {s.kind === "baseline" ? "Baseline · learns for 7 checks" : "Threshold"}
          </Badge>
        </div>
        <p className="font-medium">{s.name}</p>
        <p className="text-sm">
          <RichText text={s.definition} />
        </p>
        <div className="space-y-1">
          <p className="text-xs uppercase tracking-wide text-muted-foreground">Why this matters</p>
          <p className="text-sm">
            <RichText text={s.why} />
          </p>
        </div>
        {guessedThreshold(s) && (
          <p className="text-xs text-muted-foreground">
            AI picked this number — adjust it with &quot;Edit first&quot; if it doesn&apos;t fit your data.
          </p>
        )}
        <div className="flex gap-2">
          <Button
            ref={addRef}
            size="sm"
            disabled={pending}
            onClick={() => void onAdd(s)}
            data-testid="monitor-suggest-add"
          >
            {pending ? "Adding…" : "Add"}
          </Button>
          <Button
            size="sm"
            variant="outline"
            disabled={pending}
            onClick={() => onEditFirst(s)}
            data-testid="monitor-suggest-edit"
          >
            Edit first
          </Button>
          <Button size="sm" variant="ghost" disabled={pending} onClick={advance} data-testid="monitor-suggest-skip">
            Skip
          </Button>
        </div>
        {addError && (
          <p className="text-sm text-destructive" role="alert">
            {addError}
          </p>
        )}
      </div>
    );
  }

  function renderSummary() {
    return (
      <div className="space-y-3" data-testid="monitor-suggest-summary">
        {added.length === 0 ? (
          <>
            <p className="text-sm">No monitors added.</p>
            <div className="flex gap-2">
              <Button size="sm" variant="outline" onClick={() => void run("")}>
                Suggest again
              </Button>
              <Button ref={doneRef} size="sm" onClick={() => onOpenChange(false)} data-testid="monitor-suggest-done">
                Done
              </Button>
            </div>
          </>
        ) : (
          <>
            <h3 className="font-medium">
              Added {added.length} monitor{added.length === 1 ? "" : "s"}
            </h3>
            <ul className="space-y-2">
              {added.map((a) => (
                <li key={a.id} className="space-y-0.5 text-sm">
                  <div className="flex items-center justify-between gap-2">
                    <span>✓ {a.name}</span>
                    <Link to={`/monitors/${a.id}`} className="text-primary underline">
                      View
                    </Link>
                  </div>
                  {!a.subscribed && (
                    <p className="text-xs text-muted-foreground">
                      Added, but we couldn&apos;t subscribe you — add yourself on the monitor page.
                    </p>
                  )}
                </li>
              ))}
            </ul>
            <p className="text-sm text-muted-foreground">
              You&apos;ll get an email when they fire. Add teammates on each monitor&apos;s page.
            </p>
            <Button
              ref={doneRef}
              size="sm"
              className="w-full"
              onClick={() => onOpenChange(false)}
              data-testid="monitor-suggest-done"
            >
              Done
            </Button>
          </>
        )}
      </div>
    );
  }

  function renderBody() {
    switch (body.kind) {
      case "loading":
        return (
          <div className="flex items-center gap-2 text-sm text-muted-foreground" data-testid="monitor-suggest-loading">
            <Loader size={16} />
            Reading your query and its result…
          </div>
        );
      case "reason":
        return <p className="text-sm text-muted-foreground">{body.text}</p>;
      case "ai-error":
        return (
          <div className="space-y-2 text-sm" data-testid="monitor-suggest-error">
            <p className="text-destructive">{body.text}</p>
            {body.retry && (
              <Button size="sm" variant="outline" onClick={() => void run(lastPrompt.current)}>
                Try again
              </Button>
            )}
          </div>
        );
      case "error":
        return (
          <div className="space-y-2 text-sm" data-testid="monitor-suggest-error">
            <p className="font-medium">Couldn&apos;t get suggestions</p>
            <p className="break-words text-muted-foreground">{trimError(body.text).short}</p>
            <Button size="sm" variant="outline" onClick={() => void run(lastPrompt.current)}>
              Try again
            </Button>
          </div>
        );
      case "ok":
        if (body.suggestions.length === 0) {
          if (body.unsupported != null) {
            return (
              <div
                className="space-y-1 rounded-md border border-border bg-muted/40 p-3 text-sm"
                data-testid="monitor-suggest-unsupported"
              >
                <p className="font-medium">Can&apos;t build that from this query</p>
                <p>{body.unsupported}</p>
                <p className="text-muted-foreground">
                  Rephrase it, or edit the query so its result includes what you want to watch.
                </p>
              </div>
            );
          }
          return body.covered.length > 0 ? (
            <div data-testid="monitor-suggest-covered">
              <EmptyState
                icon={BellRing}
                title="Already covered"
                description="Existing monitors on this query already watch everything AI would suggest. Describe something specific above to add more."
              />
            </div>
          ) : (
            <div data-testid="monitor-suggest-empty">
              <EmptyState
                icon={Sparkles}
                title="Nothing obvious to monitor"
                description="AI didn't find a column worth alerting on in this query's result. Describe what you want to watch above."
              />
            </div>
          );
        }
        return done || !current ? renderSummary() : renderCard(current, body);
    }
  }

  return (
    <Sheet open={open} onOpenChange={onOpenChange}>
      <SheetContent
        side="right"
        className="w-full sm:w-[28rem] sm:max-w-[28rem]"
        data-testid="monitor-suggest-pane"
        onOpenAutoFocus={(e) => {
          e.preventDefault();
          promptRef.current?.focus();
        }}
      >
        <SheetHeader className="space-y-1 pb-0">
          <SheetTitle>Suggest monitors</SheetTitle>
          <SheetDescription>for &quot;{queryName}&quot;</SheetDescription>
        </SheetHeader>
        <div className="space-y-2 px-4 pt-3">
          <div className="flex gap-2">
            <Input
              ref={promptRef}
              value={prompt}
              maxLength={500}
              aria-label="Describe a monitor"
              placeholder="Or describe one, e.g. alert me when refund rate passes 5%"
              data-testid="monitor-suggest-prompt"
              onChange={(e) => setPrompt(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter" && !e.nativeEvent.isComposing && !loading) {
                  e.preventDefault();
                  void run(prompt);
                }
              }}
            />
            <Button
              size="sm"
              variant="outline"
              disabled={loading}
              onClick={() => void run(prompt)}
              data-testid="monitor-suggest-submit"
            >
              Suggest
            </Button>
          </div>
          <p className="text-xs text-muted-foreground" data-testid="monitor-ai-disclosure">
            AI sees this query&apos;s SQL and result columns and, unless an admin turned off sample values in{" "}
            <AiSettingsLink isAdmin={has("admin")} />, its first 5 result rows.
          </p>
          {hasUnpublishedChanges && (
            <p className="text-xs text-muted-foreground">
              Suggestions use the published version. Your unpublished edits aren&apos;t included.
            </p>
          )}
        </div>
        <div className="mt-3 flex-1 overflow-y-auto border-t border-border p-4">{renderBody()}</div>
        <p className="sr-only" aria-live="polite">
          {announce}
        </p>
      </SheetContent>
    </Sheet>
  );
}
