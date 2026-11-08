import { Sparkles } from "lucide-react";
import { formatNumeric } from "@/lib/formatNumber";
import { splitExplanation } from "@/lib/monitors";
import { exactTs, timeAgo, useNow } from "@/lib/scheduling";
import type { MonitorObservation } from "@/types/api";

interface Props {
  observation: MonitorObservation & { explanation: string };
  onViewRun: (runId: number) => void;
}

export function LatestIncidentCard({ observation, onViewRun }: Props) {
  const now = useNow(false);
  const { stats, ai, confidence } = splitExplanation(observation.explanation);
  const ms = Date.parse(observation.observed_at);
  return (
    <section
      aria-labelledby="latest-incident-heading"
      className="space-y-2 rounded-md border border-border p-3"
      data-testid="latest-incident"
    >
      <div className="flex flex-wrap items-baseline justify-between gap-2">
        <h2 id="latest-incident-heading" className="text-sm font-medium">Latest incident</h2>
        <time dateTime={observation.observed_at} title={exactTs(ms)} className="text-xs text-muted-foreground">
          {timeAgo(ms, now)}
        </time>
      </div>
      <p className="text-sm">
        Observed{" "}
        <span className="font-medium tabular-nums">
          {observation.value != null ? formatNumeric(observation.value) : "—"}
        </span>
      </p>
      <p className="whitespace-pre-line break-words text-sm text-foreground">{stats}</p>
      {ai && (
        <div className="rounded-md bg-muted px-3 py-2" data-testid="latest-incident-ai">
          <p className="flex items-center gap-1.5 text-xs font-medium text-muted-foreground">
            <Sparkles className="size-3.5" aria-hidden="true" /> AI analysis · {confidence} confidence
          </p>
          <p className="mt-1 whitespace-pre-line break-words text-sm">{ai}</p>
        </div>
      )}
      {observation.run_id != null && (
        <button
          type="button"
          className="text-sm text-primary underline underline-offset-2"
          onClick={() => onViewRun(observation.run_id as number)}
        >
          View run
        </button>
      )}
    </section>
  );
}
