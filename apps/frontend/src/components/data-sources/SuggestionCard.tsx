import { useEffect, useId, useState, type ReactNode } from "react";
import { useMutation } from "@tanstack/react-query";
import { Link } from "react-router";
import { AlertTriangle, CheckCircle, XCircle } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Loader } from "@/components/ui/loader";
import { ReadonlyField } from "@/components/common/ReadonlyField";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { api, apiErrorMessage } from "@/lib/api";
import { usePollJob } from "@/lib/jobs";
import {
  createMonitorFromSql,
  formatComparisonValue,
  opLabel,
  OPS,
  PartialCheckError,
  type MonitorKind,
  type PartialCheck,
} from "@/lib/monitors";
import { MonitorKindToggle } from "@/components/monitors/MonitorKindToggle";
import type { MonitorSuggestion, QueryResult } from "@/types/api";

const KIND_LABELS: Record<MonitorSuggestion["kind"], string> = {
  freshness: "Freshness",
  null_rate: "Null rate",
  uniqueness: "Uniqueness",
  row_count: "Row count",
};

export type CreateStatus =
  | { kind: "idle" }
  | { kind: "creating" }
  | { kind: "created"; monitorId: number; subscribed: boolean }
  | { kind: "failed"; error: string; partial?: PartialCheck };

/** The scalar comparison `evaluate_state` does server-side; used only to flag a `Test` result that
 * already trips the condition. */
export function evaluatesTrue(value: number, op: string, threshold: number): boolean {
  switch (op) {
    case ">":
      return value > threshold;
    case "<":
      return value < threshold;
    case ">=":
      return value >= threshold;
    case "<=":
      return value <= threshold;
    case "==":
      return value === threshold;
    case "!=":
      return value !== threshold;
    default:
      return false;
  }
}

const mono = (text: string) => <span className="font-mono">{text}</span>;

/** What the number means in terms of the schema column (not the SQL alias); `qualified` adds the
 * table. */
function measureLabel(s: MonitorSuggestion, qualified: boolean): ReactNode {
  const t = s.table;
  const c = s.target_column;
  const tc = qualified ? mono(`${t}.${c}`) : mono(c);
  switch (s.kind) {
    case "row_count":
      return qualified ? <>Rows in {mono(t)}</> : "Row count";
    case "freshness":
      if (!c) return qualified ? <>Hours since the newest row in {mono(t)}</> : "Hours since the newest row";
      return <>Hours since the newest {tc}</>;
    case "uniqueness":
      if (!c) return qualified ? <>Duplicate rows in {mono(t)}</> : "Duplicate row count";
      return qualified ? <>Duplicate values in {tc}</> : <>Duplicate {tc} count</>;
    case "null_rate":
      if (!c) return qualified ? <>% null in {mono(t)}</> : "Null %";
      return qualified ? <>% of {tc} that is null</> : <>Null % in {tc}</>;
  }
}

/** The first row's numeric value; separates "no first row" from "first value is not numeric",
 * which read as different problems. */
function testOutcome(result: QueryResult, column: string): { value: number } | { error: string } {
  const raw = result.data.rows[0]?.[column];
  const num = typeof raw === "number" ? raw : Number(raw);
  if (Number.isFinite(num)) return { value: num };
  return {
    error:
      result.data.rows.length === 0
        ? "The query returned no rows, so there's no first row to check."
        : "This check's SQL didn't return a usable value.",
  };
}

export { PartialCheckError };
export type { PartialCheck };

/** The accept chain for a suggestion (see `createMonitorFromSql`); exported so the parent's batch
 * loop runs the same chain. */
export function createSuggestedCheck(
  dataSourceId: number,
  suggestion: MonitorSuggestion,
  op: string,
  value: number,
  resume: PartialCheck = {},
  monitorKind: MonitorKind = "threshold",
): Promise<{ queryId: number; monitorId: number; subscribed: boolean }> {
  return createMonitorFromSql(
    {
      dataSourceId,
      queryName: suggestion.title,
      description: suggestion.rationale,
      sql: suggestion.sql,
      tags: ["data quality"],
      monitorName: suggestion.title,
      options:
        monitorKind === "baseline"
          ? { column: suggestion.column, kind: "baseline" }
          : { column: suggestion.column, op, value },
    },
    resume,
  );
}

interface SuggestionCardProps {
  dataSourceId: number;
  suggestion: MonitorSuggestion;
  op: string;
  value: string;
  monitorKind: MonitorKind;
  onMonitorKindChange: (kind: MonitorKind) => void;
  onOpChange: (op: string) => void;
  onValueChange: (value: string) => void;
  selected: boolean;
  onSelectedChange: (selected: boolean) => void;
  createStatus: CreateStatus;
  onCreateStatusChange: (status: CreateStatus) => void;
  onTestFailedChange: (failed: boolean) => void;
  onDismiss: () => void;
}

/** One suggestion card. Draft, selection and creation status are controlled by the parent
 * `DataQualityChecksCard`; this card owns only its `Test` job and the "SQL didn't run" gate on `Create check`. */
export function SuggestionCard({
  dataSourceId,
  suggestion,
  op,
  value,
  monitorKind,
  onMonitorKindChange,
  onOpChange,
  onValueChange,
  selected,
  onSelectedChange,
  createStatus,
  onCreateStatusChange,
  onTestFailedChange,
  onDismiss,
}: SuggestionCardProps) {
  const [testResult, setTestResult] = useState<
    { value: number } | { error: string } | { unknown: string } | null
  >(null);
  const [testJobId, setTestJobId] = useState<string | null>(null);
  const job = usePollJob(testJobId);

  const testMutation = useMutation({
    mutationFn: () => api.executeAdHoc({ data_source_id: dataSourceId, query: suggestion.sql }),
    onMutate: () => {
      setTestResult(null);
      setTestJobId(null);
    },
    onSuccess: (res) => {
      if ("query_result" in res && res.query_result) {
        setTestResult(testOutcome(res.query_result, suggestion.column));
      } else if ("job" in res && res.job) {
        setTestJobId(res.job.id);
      }
    },
    onError: (err) => setTestResult({ error: apiErrorMessage(err, "This SQL didn't run") }),
  });

  useEffect(() => {
    if (testJobId == null || !job.isDone) return;
    if (job.isSuccess && job.queryResultId != null) {
      api
        .getQueryResult(job.queryResultId)
        .then((res) => setTestResult(testOutcome(res.query_result, suggestion.column)))
        .catch((err) => setTestResult({ error: apiErrorMessage(err, "This SQL didn't run") }));
    } else if (job.isFailure) {
      setTestResult({ error: job.error ?? "This SQL didn't run" });
    } else if (job.gaveUp) {
      // A poll timeout is an unknown outcome (the SQL may still run), kept out of `error` so it
      // does not poison `canCreate`.
      setTestResult({ unknown: job.error ?? "We lost track of this test run." });
    } else {
      // A done job can report success without a result id; handle it or the spinner vanishes and a
      // passed test looks lost.
      setTestResult({ unknown: "This test finished without returning a result." });
    }
    setTestJobId(null);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [job.isDone, job.isSuccess, job.isFailure, job.gaveUp, job.queryResultId, testJobId]);

  const testFailed = testResult != null && "error" in testResult;

  // A suggestion whose SQL failed its test cannot make a working check; report it so an earlier
  // checked box gets unchecked.
  useEffect(() => {
    onTestFailedChange(testFailed);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [testFailed]);

  const kindLabelId = useId();
  const isBaseline = monitorKind === "baseline";
  const valueValid = isBaseline || (value.trim() !== "" && Number.isFinite(Number(value)));
  const canCreate = valueValid && !testFailed && createStatus.kind !== "creating";

  async function handleCreate() {
    if (!canCreate) return;
    const resume = createStatus.kind === "failed" ? (createStatus.partial ?? {}) : {};
    onCreateStatusChange({ kind: "creating" });
    try {
      const { monitorId, subscribed } = await createSuggestedCheck(
        dataSourceId,
        suggestion,
        op,
        Number(value),
        resume,
        monitorKind,
      );
      onCreateStatusChange({ kind: "created", monitorId, subscribed });
    } catch (err) {
      onCreateStatusChange(
        err instanceof PartialCheckError
          ? { kind: "failed", error: err.message, partial: err.partial }
          : { kind: "failed", error: apiErrorMessage(err, "Failed to create check") },
      );
    }
  }

  const tested = testResult != null && "value" in testResult ? testResult.value : null;
  const measured = tested != null && (
    <>
      {measureLabel(suggestion, false)} is {formatComparisonValue(String(tested))}
    </>
  );
  const alertsNow = tested != null && evaluatesTrue(tested, op, Number(value));

  if (createStatus.kind === "created") {
    return (
      <div className="flex items-center justify-between gap-3 rounded-md border border-border bg-muted/30 px-3 py-2 text-sm">
        <span>
          <span className="text-success">✓</span> <span className="font-medium">{suggestion.title}</span>
          {!createStatus.subscribed && (
            <span className="ml-2 text-xs text-muted-foreground">
              Check created, but we couldn't subscribe you — add yourself on the monitor page.
            </span>
          )}
        </span>
        <Link to={`/monitors/${createStatus.monitorId}`} className="text-primary underline shrink-0">
          View monitor
        </Link>
      </div>
    );
  }

  return (
    <div className="relative space-y-3 rounded-md border border-border p-3">
      <label className="absolute right-3 top-3 flex items-center gap-1.5 text-xs text-muted-foreground">
        <input
          type="checkbox"
          className="size-3.5 cursor-pointer accent-primary"
          checked={selected}
          disabled={!canCreate}
          onChange={(e) => onSelectedChange(e.target.checked)}
          aria-label={`Select ${suggestion.title}`}
        />
        Select
      </label>

      <div className="pr-16">
        <span className="inline-block rounded-full bg-muted px-2 py-0.5 text-xs font-medium text-muted-foreground">
          {KIND_LABELS[suggestion.kind]}
        </span>
        <p className="mt-1 font-medium">{suggestion.title}</p>
      </div>

      <p className="text-sm text-muted-foreground">{suggestion.rationale}</p>

      {suggestion.kind === "row_count" && (
        <div>
          <p id={`sugg-kind-${kindLabelId}`} className="text-sm font-medium">Alert type</p>
          <MonitorKindToggle
            value={monitorKind}
            onChange={onMonitorKindChange}
            labelId={`sugg-kind-${kindLabelId}`}
          />
        </div>
      )}

      <div className="grid grid-cols-1 sm:grid-cols-3 gap-3">
        <ReadonlyField label="Measures" value={measureLabel(suggestion, true)} />
        {isBaseline ? (
          <p className="text-xs text-muted-foreground sm:col-span-2">
            Alerts when the row count moves well outside its recent pattern. Starts alerting after 7 checks.
          </p>
        ) : (
          <>
            <div className="space-y-1">
              <Label htmlFor={`sugg-op-${kindLabelId}`}>Operator</Label>
              <Select value={op} onValueChange={onOpChange}>
                <SelectTrigger id={`sugg-op-${kindLabelId}`}>
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  {OPS.map((o) => (
                    <SelectItem key={o} value={o}>
                      {o}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
            </div>
            <div className="space-y-1">
              <Label htmlFor={`sugg-val-${kindLabelId}`}>Value</Label>
              <Input
                id={`sugg-val-${kindLabelId}`}
                type="number"
                value={value}
                onChange={(e) => onValueChange(e.target.value)}
                aria-invalid={!valueValid}
                aria-describedby={valueValid ? undefined : `sugg-val-err-${kindLabelId}`}
              />
              {!valueValid && (
                <p id={`sugg-val-err-${kindLabelId}`} className="text-xs text-destructive">
                  Enter a number
                </p>
              )}
              {suggestion.guessed_threshold && <p className="text-xs text-muted-foreground">Adjust to fit your data.</p>}
            </div>
          </>
        )}
      </div>

      <details>
        <summary className="cursor-pointer text-xs text-muted-foreground underline">Show SQL</summary>
        <pre className="mt-1 overflow-x-auto rounded-md bg-background p-3 text-xs">{suggestion.sql}</pre>
      </details>

      <div className="text-sm">
        {testMutation.isPending || (testJobId != null && !job.isDone) ? (
          <span className="flex items-center gap-1.5 text-muted-foreground">
            <Loader size={14} /> Testing…
          </span>
        ) : tested != null && isBaseline ? (
          <span role="status" className="text-sm text-muted-foreground">
            {measured} now. Baseline checks learn from their first 7 checks before they can alert.
          </span>
        ) : tested != null && !valueValid ? (
          <span role="status" className="flex items-start gap-1.5 text-sm text-muted-foreground">
            <AlertTriangle className="size-4 shrink-0 text-warning" aria-hidden="true" />
            <span>{measured}. Set a threshold above to see whether this check would alert.</span>
          </span>
        ) : tested != null ? (
          <span
            role="status"
            className={`flex items-start gap-1.5 text-sm ${alertsNow ? "text-warning-foreground" : "text-muted-foreground"}`}
          >
            {alertsNow ? (
              <AlertTriangle className="size-4 shrink-0 text-warning" aria-hidden="true" />
            ) : (
              <CheckCircle className="size-4 shrink-0 text-success" aria-hidden="true" />
            )}
            <span>
              {alertsNow ? "Would alert now." : "No alert."} {measured}. This check alerts when it{" "}
              {opLabel(op).phrase} {formatComparisonValue(String(Number(value)))}.
            </span>
          </span>
        ) : testResult != null && "error" in testResult ? (
          <span role="status" className="flex items-start gap-1.5 text-sm text-destructive">
            <XCircle className="size-4 shrink-0" aria-hidden="true" />
            <span>Can&apos;t evaluate. {testResult.error}</span>
          </span>
        ) : testResult != null && "unknown" in testResult ? (
          <span role="status" className="flex items-center gap-1.5 text-muted-foreground">
            <Loader size={14} /> {testResult.unknown}
          </span>
        ) : null}
      </div>

      {createStatus.kind === "failed" && (
        <p className="text-sm text-destructive">{createStatus.error}</p>
      )}

      <div className="flex items-center gap-2">
        <Button size="sm" onClick={() => testMutation.mutate()} disabled={testMutation.isPending}>
          {testFailed ? "Retry" : "Test"}
        </Button>
        <Button size="sm" variant="outline" onClick={handleCreate} disabled={!canCreate}>
          {createStatus.kind === "creating" ? "Creating…" : "Create check"}
        </Button>
        <Button size="sm" variant="ghost" onClick={onDismiss}>
          Dismiss
        </Button>
      </div>
    </div>
  );
}
