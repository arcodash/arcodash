import { cadenceLabel, exactTs, isFailedStatus, isStale, timeAgo, useNow } from "@/lib/scheduling";
import type { WidgetFreshness } from "@/types/api";

interface TileFreshnessDotProps {
  freshness: WidgetFreshness;
  /** Retained for callers; the age text is always shown. */
  alwaysShowAge?: boolean;
}

/** The tile's freshness dot: a pure reader of the widget DTO's `freshness` fields. Neutral (fresh),
 * amber (stale), red (failed; the last-good number stays, only the dot changes). */
export function TileFreshnessDot({ freshness }: TileFreshnessDotProps) {
  const now = useNow(true);
  const retrievedMs = Date.parse(freshness.retrieved_at);
  const ago = timeAgo(retrievedMs, now);
  const exact = exactTs(retrievedMs);

  const failed = isFailedStatus(freshness.last_run_status);
  const ageSecs = (now - retrievedMs) / 1000;
  const stale = !failed && isStale(ageSecs, freshness.expected_interval_secs);

  let containerClass = "text-muted-foreground";
  let dotClass = "bg-muted-foreground/40";
  let text = `Updated ${ago}`;
  let title = `Updated ${ago} (${exact})`;

  if (stale) {
    containerClass = "rounded bg-warning/10 px-1 text-warning-foreground";
    dotClass = "bg-warning";
    text =
      freshness.expected_interval_secs != null
        ? `${ago} · expected ${cadenceLabel(freshness.expected_interval_secs)}`
        : ago;
  } else if (failed) {
    containerClass = "text-destructive";
    dotClass = "bg-destructive";
    text = `Data may be outdated · ${ago}`;
    title = `Showing last successful result from ${ago} (${exact})`;
  }

  return (
    <span
      className={`flex shrink-0 items-center gap-1.5 text-xs ${containerClass}`}
      title={title}
      aria-label={title}
    >
      <span className={`h-2 w-2 shrink-0 rounded-full ${dotClass}`} />
      <span>{text}</span>
    </span>
  );
}
