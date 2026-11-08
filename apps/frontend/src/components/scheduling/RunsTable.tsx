/** Last-≤10 runs with an inline error expander and per-run CSV link. Query shows 5 columns; monitor shows 8,
 * split into Check (ran), Result (condition matched), Notification (delivered) so a healthy row never reads as a contradiction. */
import { Fragment, type ReactNode } from "react";
import { Loader } from "@/components/ui/loader";
import { Button } from "@/components/ui/button";
import { api } from "@/lib/api";
import { cn } from "@/lib/utils";
import {
  elapsed,
  formatDuration,
  isFailedRun,
  isInFlight,
  isStaleFallback,
  runStartMs,
  timeAgo,
  exactTs,
} from "@/lib/scheduling";
import type { Run } from "@/types/api";

interface RunsTableProps {
  entityType: "query" | "monitor";
  runs: Run[];
  now: number;
  openErrorRunId: number | null;
  onToggleError: (runId: number | null) => void;
  onRetry: () => void;
  canRetry: boolean;
  /** monitor only — drives the stale-fallback amber on the Data-age cell. */
  maxFreshWaitSecs?: number | null;
}

function Dotted({ dot, className, children }: { dot: string; className?: string; children: ReactNode }) {
  return (
    <span className={cn("flex items-center gap-1.5", className)}>
      <span className={`size-1.5 rounded-full ${dot}`} />
      {children}
    </span>
  );
}

/** A monitor check that produced a result but could not compute the condition (missing column, non-numeric
 * value), unlike one that has not resolved yet. See `evaluate_monitor.rs::evaluate_state` `"unevaluable"`. */
function isUnevaluableRun(run: Run): boolean {
  return run.status === "success" && run.condition_met == null && run.error != null;
}

/** Query runs show plain status; on monitors this is column 1, "did the check execute", and never says
 * whether the condition matched ("Alerting" is the monitor's overall state). */
function StatusCell({ run, isMonitor }: { run: Run; isMonitor: boolean }) {
  if (isInFlight(run)) {
    return (
      <span className="flex items-center gap-1.5 text-muted-foreground">
        <Loader size={14} />
        {!isMonitor ? "Running" : run.waiting_on_run_id != null ? "Waiting…" : "Checking…"}
      </span>
    );
  }
  if (run.status === "success") {
    return <Dotted dot={isMonitor ? "bg-success" : "bg-green-500"}>{isMonitor ? "Ran" : "Success"}</Dotted>;
  }
  if (isFailedRun(run)) {
    return (
      <Dotted dot="bg-destructive" className="text-destructive">
        {run.status === "timed_out" ? "Timed out" : isMonitor ? "Failed to run" : "Failed"}
      </Dotted>
    );
  }
  // An unrecognized status shows verbatim rather than being mislabeled as "Skipped".
  const label = run.status === "cancelled" ? "Cancelled" : run.status === "skipped" ? "Skipped" : run.status;
  return <span className="text-muted-foreground">{label}</span>;
}

/** Column 2: did the condition match. "Couldn't evaluate" (hollow dot) must not read as met or not
 * met. */
function ResultCell({ run }: { run: Run }) {
  if (run.condition_met === true) {
    return (
      <Dotted dot="bg-warning" className="text-warning-foreground">
        Condition met
      </Dotted>
    );
  }
  if (run.condition_met === false) return <Dotted dot="bg-success">Condition not met</Dotted>;
  if (isUnevaluableRun(run)) {
    return (
      <Dotted dot="border border-muted-foreground" className="text-muted-foreground">
        Couldn&apos;t evaluate
      </Dotted>
    );
  }
  return <span className="text-muted-foreground">—</span>;
}

const TOOLTIP_ERROR_MAX_LEN = 200;

/** Caps a delivery error at a tooltip-friendly length (raw provider errors can run to paragraphs).
 * Display only. */
function truncateForTooltip(text: string): string {
  return text.length > TOOLTIP_ERROR_MAX_LEN ? `${text.slice(0, TOOLTIP_ERROR_MAX_LEN)}…` : text;
}

/** Per-destination summary; never collapses independent deliveries, so a retrying webhook next to a sent
 * email does not read as plain "Sent". */
function notifiedCell(run: Run) {
  const notified = run.notified;
  if (notified.length === 0) {
    return <span className="text-muted-foreground">Not sent — state unchanged</span>;
  }

  const tooltip = notified
    .map((n) => `${n.destination_name}: ${n.status}${n.error ? ` (${truncateForTooltip(n.error)})` : ""}`)
    .join(", ");

  const groups = [
    { key: "sent", prefix: "Sent to", items: notified.filter((n) => n.status === "sent") },
    {
      key: "failed",
      prefix: "Failed:",
      items: notified.filter((n) => n.status === "failed"),
      className: "text-destructive",
    },
    {
      key: "pending",
      prefix: "Sending to",
      items: notified.filter((n) => n.status !== "sent" && n.status !== "failed"),
      className: "text-muted-foreground",
      suffix: "…",
    },
  ];
  const segments = groups
    .filter((g) => g.items.length > 0)
    .map(({ key, prefix, items, className, suffix }) => (
      <span key={key} className={className}>
        {prefix} {items[0].destination_name}
        {items.length > 1 ? ` +${items.length - 1}` : ""}
        {suffix}
      </span>
    ));

  return (
    <span title={tooltip}>
      {segments.map((seg, i) => (i === 0 ? seg : <span key={`sep-${i}`}>; {seg}</span>))}
    </span>
  );
}

function dataAgeCell(run: Run, maxFreshWaitSecs: number | null | undefined) {
  if (run.data_age_secs == null) return <span className="text-muted-foreground">—</span>;
  const age = formatDuration(run.data_age_secs * 1000);
  if (isStaleFallback(run.data_age_secs, maxFreshWaitSecs)) {
    return (
      <span
        className="text-warning-foreground"
        title={`The refresh is taking longer than usual. This check ran against data from ${age} ago — we'll re-check as soon as fresh data is ready.`}
      >
        data {age} old
      </span>
    );
  }
  return <span className="text-muted-foreground">data {age} old</span>;
}

export function RunsTable({
  entityType,
  runs,
  now,
  openErrorRunId,
  onToggleError,
  onRetry,
  canRetry,
  maxFreshWaitSecs,
}: RunsTableProps) {
  const isMonitor = entityType === "monitor";
  const colSpan = isMonitor ? 8 : 6;

  if (runs.length === 0) {
    return (
      <p className="py-2 text-xs text-muted-foreground">
        {isMonitor
          ? "No checks yet — the first check will appear here."
          : "No runs yet — the first refresh will appear here."}
      </p>
    );
  }

  return (
    <div>
      <table className="w-full text-xs">
        {isMonitor && (
          <thead>
            <tr className="border-b border-border text-left text-muted-foreground">
              <th className="py-1.5 pr-2 font-normal">Check</th>
              <th className="py-1.5 pr-2 font-normal">When</th>
              <th className="py-1.5 pr-2 font-normal">Duration</th>
              <th className="py-1.5 pr-2 font-normal">Result</th>
              <th className="py-1.5 pr-2 font-normal">Data age</th>
              <th className="py-1.5 pr-2 font-normal">Notification</th>
              <th className="py-1.5 pr-2 font-normal">CSV</th>
              <th className="py-1.5 font-normal">
                <span className="sr-only">Details</span>
              </th>
            </tr>
          </thead>
        )}
        <tbody>
          {runs.map((run) => {
            const startedMs = runStartMs(run);
            const isFailedRow = isFailedRun(run);
            // Failed and "unevaluable" runs both carry a reason in `run.error`; the ⓘ column
            // expands either.
            const hasExpandableReason = isFailedRow || (isMonitor && isUnevaluableRun(run));
            return (
              <Fragment key={run.id}>
                <tr className="border-t border-border">
                  <td className="py-1.5 pr-2">
                    <StatusCell run={run} isMonitor={isMonitor} />
                  </td>
                  <td
                    className="py-1.5 pr-2 text-muted-foreground"
                    title={startedMs != null ? exactTs(startedMs) : undefined}
                  >
                    {startedMs != null ? timeAgo(startedMs, now) : "—"}
                  </td>
                  <td className="py-1.5 pr-2 text-muted-foreground">
                    {isInFlight(run) && startedMs != null
                      ? elapsed(startedMs, now)
                      : run.duration_ms != null
                        ? formatDuration(run.duration_ms)
                        : "—"}
                  </td>
                  {isMonitor ? (
                    <>
                      <td className="py-1.5 pr-2">
                        <ResultCell run={run} />
                      </td>
                      <td className="py-1.5 pr-2">{dataAgeCell(run, maxFreshWaitSecs)}</td>
                      <td className="py-1.5 pr-2">{notifiedCell(run)}</td>
                    </>
                  ) : (
                    <td className="py-1.5 pr-2 text-muted-foreground">
                      {run.row_count != null ? `${run.row_count.toLocaleString()} rows` : ""}
                    </td>
                  )}
                  <td className="py-1.5 pr-2">
                    {run.query_result_id != null && (
                      <a
                        href={api.runResultCsvUrl(run.id)}
                        download
                        className="text-primary hover:underline"
                      >
                        ⬇ CSV
                      </a>
                    )}
                  </td>
                  <td className="py-1.5">
                    {hasExpandableReason && (
                      <button
                        type="button"
                        aria-label={isFailedRow ? "Show error" : "Show reason"}
                        className="cursor-pointer text-muted-foreground hover:text-foreground"
                        onClick={() => onToggleError(openErrorRunId === run.id ? null : run.id)}
                      >
                        ⓘ
                      </button>
                    )}
                  </td>
                </tr>
                {hasExpandableReason && openErrorRunId === run.id && (
                  <tr>
                    <td colSpan={colSpan} className="pb-2">
                      <div className="space-y-2 rounded-md border border-border bg-muted/40 p-3">
                        <pre className="overflow-x-auto rounded-md bg-background p-3 text-xs">
                          {run.error}
                        </pre>
                        <Button size="sm" variant="outline" onClick={onRetry} disabled={!canRetry}>
                          Retry
                        </Button>
                      </div>
                    </td>
                  </tr>
                )}
              </Fragment>
            );
          })}
        </tbody>
      </table>
      <p className="pt-2 text-xs text-muted-foreground">
        {isMonitor
          ? "Showing the last 10 checks (older checks aren't retained). Each check reads the first row of the result."
          : "Showing the last 10 runs (older runs aren't retained)."}
      </p>
    </div>
  );
}
