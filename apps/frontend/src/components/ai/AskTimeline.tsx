import { useMemo, useState } from "react";
import { Link } from "react-router";
import { ChevronDown, ChevronRight } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Loader } from "@/components/ui/loader";
import { ResultsTable } from "@/components/query-editor/ResultsTable";
import type { ResultData } from "@/types/api";
import { previewLabel, trimError, type AskPreview, type AskRunState, type AskStep } from "./askRun";

function ErrorText({ raw }: { raw: string }) {
  const [full, setFull] = useState(false);
  const { short, truncated } = trimError(raw);
  return (
    <span className="min-w-0 break-words">
      {full ? <span className="whitespace-pre-wrap">{raw}</span> : short}
      {truncated && (
        <button
          type="button"
          className="ml-1.5 cursor-pointer text-xs underline"
          onClick={() => setFull((v) => !v)}
        >
          {full ? "Hide full error" : "Show full error"}
        </button>
      )}
    </span>
  );
}

export function ExhaustedWarning({ problem }: { problem: string }) {
  return (
    <p
      role="alert"
      className="rounded-md border border-warning/40 bg-warning/10 px-3 py-2 text-sm text-warning"
    >
      This SQL didn&apos;t run successfully (last error: <ErrorText raw={problem} />). Apply anyway
      to edit it by hand.
    </p>
  );
}

function DraftEntry({ attempt, sql }: { attempt: number; sql: string }) {
  const [open, setOpen] = useState(false);
  return (
    <div>
      <button
        type="button"
        aria-expanded={open}
        className="flex cursor-pointer items-center gap-1 text-left hover:text-foreground"
        onClick={() => setOpen((v) => !v)}
      >
        {open ? <ChevronDown className="size-3" /> : <ChevronRight className="size-3" />}
        Draft {attempt}
      </button>
      {open && (
        <pre className="mt-1 max-h-40 overflow-auto rounded-md border border-border bg-muted/40 p-2 font-mono text-xs whitespace-pre-wrap">
          {sql}
        </pre>
      )}
    </div>
  );
}

function StepRow({ step }: { step: AskStep }) {
  switch (step.kind) {
    case "status":
      return <div>{step.text}</div>;
    case "draft":
      return <DraftEntry attempt={step.attempt} sql={step.sql} />;
    case "executing":
      return <div>Running preview</div>;
    case "repair":
      return (
        <div className="flex gap-1 text-destructive">
          <span className="shrink-0">Error:</span>
          <ErrorText raw={step.error} />
          <span className="shrink-0">→ repairing</span>
        </div>
      );
    case "preview":
      return <div>Preview: {previewLabel(step)}</div>;
    case "refreshing":
      return (
        <div data-testid="ask-step-refreshing">
          A table or column changed since AI last looked. Updating the schema…
        </div>
      );
    case "retrying":
      return <div data-testid="ask-step-retrying">Schema updated · asking again</div>;
    case "visualizing":
      return <div>Choosing a chart</div>;
    case "suggesting":
      return <div>Drafting the monitor</div>;
  }
}

function summaryLine(state: AskRunState): string {
  const attempts = state.final?.attempts ?? 1;
  const n = `${attempts} ${attempts === 1 ? "attempt" : "attempts"}`;
  const parts: string[] = [];
  if (state.succeeded) {
    parts.push(`Solved in ${n}`);
    if (state.preview) parts.push(previewLabel({ shown: state.preview.rows.length, ...state.preview }));
  } else {
    parts.push(`Didn't run successfully after ${n}`);
  }
  if (state.final?.served_by) parts.push(state.final.served_by);
  return parts.join(" · ");
}

interface AskTimelineProps {
  state: AskRunState;
  onCancel: () => void;
  /** Header shown when the run fails; defaults to the SQL-generation wording. */
  failedTitle?: string;
}

/** Vertical step list for an Ask run; collapses to a one-line summary once solved. */
export function AskTimeline({ state, onCancel, failedTitle = "Couldn't generate SQL" }: AskTimelineProps) {
  const [expanded, setExpanded] = useState(false);
  if (state.phase === "idle") return null;

  const running = state.phase === "running";
  const collapsed = state.phase === "done" && !expanded;

  const steps = state.steps.length > 0 && (
    <ol className="mt-2 max-h-40 space-y-1 overflow-y-auto border-l border-border pl-3 text-xs text-muted-foreground">
      {state.steps.map((step, i) => (
        <li key={i}>
          <StepRow step={step} />
        </li>
      ))}
      {state.phase === "cancelled" && <li>Cancelled</li>}
    </ol>
  );

  return (
    <div
      className="rounded-md border border-border px-3 py-2 text-sm"
      data-testid="ask-timeline"
      data-phase={state.phase}
    >
      <div className="flex items-center justify-between gap-2">
        {state.phase === "done" ? (
          <button
            type="button"
            aria-expanded={expanded}
            className="flex min-w-0 cursor-pointer items-center gap-1 text-left font-medium"
            onClick={() => setExpanded((v) => !v)}
          >
            {expanded ? (
              <ChevronDown className="size-4 shrink-0" />
            ) : (
              <ChevronRight className="size-4 shrink-0" />
            )}
            <span className="truncate">{summaryLine(state)}</span>
          </button>
        ) : (
          <span className="flex items-center gap-2 font-medium">
            {running && <Loader size={14} />}
            {running
              ? state.waiting
                ? "Updating the schema…"
                : "Working on it…"
              : state.phase === "failed"
                ? state.giveUp
                  ? "Schema still updating"
                  : failedTitle
                : "Cancelled"}
          </span>
        )}
        {running && (
          <Button variant="outline" size="sm" onClick={onCancel}>
            Cancel
          </Button>
        )}
      </div>
      {!collapsed && steps}
      {state.phase === "failed" && state.giveUp && (
        <p
          role="alert"
          className="mt-2 rounded-md border border-warning/30 bg-warning/10 px-3 py-2 text-sm text-warning-foreground"
          data-testid="ask-refresh-timeout"
        >
          {state.error}
        </p>
      )}
      {state.phase === "failed" && state.error && !state.giveUp && (
        <p role="alert" className="mt-2 text-sm text-destructive">
          {state.error}
        </p>
      )}
    </div>
  );
}

/** Up to five preview rows under the SQL preview; scrolls internally. */
export function AskPreviewGrid({ preview }: { preview: AskPreview }) {
  const data = useMemo<ResultData>(() => {
    const names = preview.columns.map((c) => c.name);
    return {
      columns: preview.columns,
      rows: preview.rows.map((r) =>
        Array.isArray(r)
          ? Object.fromEntries(names.map((n, i) => [n, r[i]]))
          : (r as Record<string, unknown>),
      ),
    };
  }, [preview]);
  return (
    <div className="max-h-[clamp(96px,calc((var(--ask-avail,9999px)-188px)/2),192px)] overflow-auto rounded-md border border-border"
      data-testid="ask-preview-grid">
      {data.rows.length === 0 ? (
        <p className="px-3 py-2 text-sm text-muted-foreground">Query ran successfully — no rows</p>
      ) : (
        <ResultsTable data={data} showTypeBadges={false} />
      )}
    </div>
  );
}

/** "AI Settings" links only for admins, who control sample values. */
export function AiSettingsLink({ isAdmin }: { isAdmin: boolean }) {
  return isAdmin ? (
    <Link to="/organization/ai" className="text-primary underline">
      AI Settings
    </Link>
  ) : (
    "AI Settings"
  );
}

/** What the model is shown. */
export function AskDisclosure({ isAdmin }: { isAdmin: boolean }) {
  return (
    <p className="text-xs text-muted-foreground">
      AI sees your schema, a few sample values per column, your SQL, and the error message — never
      your query results. Admins can turn sample values off in <AiSettingsLink isAdmin={isAdmin} />.
    </p>
  );
}
