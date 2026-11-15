import { useState } from "react";
import { Sparkles } from "lucide-react";
import { formatNumeric } from "@/lib/formatNumber";
import { splitExplanation } from "@/lib/monitors";
import { exactTs, timeAgo, useNow } from "@/lib/scheduling";
import type { MonitorObservation } from "@/types/api";

interface Props {
  observation: MonitorObservation & { explanation: string };
  onViewRun: (runId: number) => void;
  /** Whether the monitor is triggered right now; otherwise the card collapses to one line. */
  firing?: boolean;
}

export function LatestIncidentCard({ observation, onViewRun, firing = true }: Props) {
  const now = useNow(false);
  const [open, setOpen] = useState(false);
  const { stats, ai, confidence } = splitExplanation(observation.explanation);
  const ms = Date.parse(observation.observed_at);
  const observed = observation.value != null ? formatNumeric(observation.value) : "—";
  if (!firing && !open) {
    return (
      <section
        aria-label="Latest incident"
        className="flex flex-wrap items-center gap-x-2 gap-y-1 rounded-md border border-border px-3 py-2 text-xs text-muted-foreground"
        data-testid="latest-incident"
      >
        <span className="font-medium text-foreground">Last incident</span>
        <span>·</span>
        <time dateTime={observation.observed_at} title={exactTs(ms)}>
          {timeAgo(ms, now)}
        </time>
        <span>·</span>
        <span>no longer firing</span>
        <span>·</span>
        <span>
          Observed <span className="tabular-nums">{observed}</span>
        </span>
        <button
          type="button"
          className="ml-auto text-primary underline underline-offset-2"
          onClick={() => setOpen(true)}
        >
          Show details
        </button>
      </section>
    );
  }
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
          {observed}
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
      {!firing && (
        <button
          type="button"
          className="mr-3 text-sm text-primary underline underline-offset-2"
          onClick={() => setOpen(false)}
        >
          Hide details
        </button>
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
