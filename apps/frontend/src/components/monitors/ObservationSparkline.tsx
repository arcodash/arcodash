import { Line, LineChart, ReferenceLine, ResponsiveContainer, Tooltip, XAxis, YAxis } from "recharts";
import { formatNumeric } from "@/lib/formatNumber";
import { formatComparisonValue, median } from "@/lib/monitors";
import { formatAlertsAt, learningSlots, type LearningStatus } from "@/lib/monitorLearning";
import { exactTs } from "@/lib/scheduling";
import type { MonitorKind, MonitorObservation } from "@/types/api";

const BOX = "flex h-16 items-center justify-center text-xs text-muted-foreground";

interface DotProps {
  cx?: number;
  cy?: number;
  payload?: MonitorObservation;
  index?: number;
}

function IncidentDot({ cx, cy, payload, index }: DotProps) {
  if (payload?.state !== "triggered" || cx == null || cy == null) return <g key={index} />;
  return <circle key={index} cx={cx} cy={cy} r={3} fill="var(--color-destructive)" />;
}

function ObsTooltip({ active, payload }: { active?: boolean; payload?: { payload: MonitorObservation }[] }) {
  const o = active ? payload?.[0]?.payload : undefined;
  if (!o) return null;
  return (
    <div className="rounded-md border bg-popover px-2 py-1 text-xs">
      <div>{exactTs(Date.parse(o.observed_at))}</div>
      <div className="tabular-nums">{o.value == null ? "No value" : formatNumeric(o.value)}</div>
    </div>
  );
}

export function ObservationSparkline({
  observations,
  kind,
  threshold,
  learning,
  learningStatus,
  nowMs = Date.now(),
  column,
  isLoading,
  isError,
  onRetry,
}: {
  observations: MonitorObservation[] | undefined;
  kind: MonitorKind;
  threshold?: number;
  learning?: { have: number; need: number } | null;
  learningStatus?: LearningStatus | null;
  nowMs?: number;
  column: string;
  isLoading?: boolean;
  isError?: boolean;
  onRetry?: () => void;
}) {
  if (isLoading) return <div className="h-16 animate-pulse rounded-md bg-muted" />;
  if (isError || !observations) {
    return (
      <p className="text-xs text-muted-foreground">
        Couldn&apos;t load recent checks.{" "}
        <button type="button" className="underline underline-offset-2" onClick={onRetry}>
          Retry
        </button>
      </p>
    );
  }
  const n = observations.length;
  const values = observations.flatMap((o) => (o.value == null ? [] : [o.value]));
  let empty: string | null = null;
  if (n === 0) empty = "No checks yet — the chart fills in after the first check.";
  else if (n === 1 && !(kind === "baseline" && learning)) empty = `1 check so far: ${values[0] == null ? "no value" : formatNumeric(values[0])}`;
  else if (values.length === 0) empty = `Recent checks couldn't read a number from ${column}.`;
  if (empty) return <div className={`rounded-md border border-border px-2 py-1 ${BOX}`}>{empty}</div>;

  const alerts = observations.filter((o) => o.state === "triggered").length;
  const isLearning = kind === "baseline" && learning != null;
  const med = kind === "baseline" && !isLearning ? median(values.slice(-30)) : null;
  const latest = formatNumeric(values[values.length - 1]);
  const refLabel = kind === "baseline" ? "median" : "threshold";
  const refValue = kind === "baseline" ? med : (threshold ?? null);
  const aria = isLearning
    ? `Recent checks: ${n} checks, latest ${latest}, learning ${learning.have} of ${learning.need}, ${alerts} alerts`
    : `Recent checks: ${n} checks, latest ${latest}${refValue != null ? `, ${refLabel} ${formatNumeric(refValue)}` : ""}, ${alerts} alerts`;
  const progress = isLearning ? `${learning.have} of ${learning.need} checks` : "";
  const learningCaption =
    learningStatus?.kind === "paused"
      ? `Learning paused: ${progress} — the check schedule is off.`
      : learningStatus?.kind === "active" && learningStatus.alertsAtMs != null
        ? `Learning: ${progress} · alerts start ~${formatAlertsAt(learningStatus.alertsAtMs, nowMs)} (${learningStatus.cadenceLabel})`
        : isLearning
          ? `Learning: ${progress}. Alerts start after ${learning.need}.`
          : "";
  const caption = isLearning
    ? learningCaption
    : kind === "baseline"
      ? `Median of last 30: ${med != null ? formatNumeric(med) : "—"} · ${n} checks · ${alerts} alerts`
      : `Threshold ${threshold != null ? formatComparisonValue(String(threshold)) : "—"} · ${n} checks · ${alerts} alerts`;

  const chartData = isLearning ? learningSlots(observations) : observations;

  return (
    <div>
      <div role="img" aria-label={aria} className="rounded-md border border-border px-2 py-1">
        <ResponsiveContainer width="100%" height={64}>
          <LineChart
            data={chartData}
            accessibilityLayer={false}
            margin={{ top: 4, right: 4, bottom: 4, left: 4 }}
          >
            {isLearning ? (
              <XAxis dataKey="slot" type="number" domain={[0, learning.need - 1]} hide />
            ) : (
              <XAxis dataKey="observed_at" hide />
            )}
            <YAxis hide domain={["auto", "auto"]} />
            <Line
              type="monotone"
              dataKey="value"
              stroke="var(--color-foreground)"
              strokeWidth={1.5}
              isAnimationActive={false}
              connectNulls={false}
              dot={<IncidentDot />}
              activeDot={false}
            />
            {refValue != null && (
              <ReferenceLine
                y={refValue}
                strokeDasharray="3 3"
                stroke={kind === "baseline" ? "var(--color-muted-foreground)" : "var(--color-destructive)"}
              />
            )}
            {isLearning && (
              <ReferenceLine
                x={learning.need - 1}
                strokeDasharray="3 3"
                stroke="var(--color-muted-foreground)"
                label={{ value: "alerts start", position: "insideTopRight", fontSize: 10, fill: "var(--color-muted-foreground)" }}
              />
            )}
            <Tooltip content={<ObsTooltip />} />
          </LineChart>
        </ResponsiveContainer>
      </div>
      <p className="mt-1 text-xs tabular-nums text-muted-foreground">{caption}</p>
    </div>
  );
}
