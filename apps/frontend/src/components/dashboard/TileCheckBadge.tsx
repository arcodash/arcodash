import { Link } from "react-router";
import type { CheckSummary } from "@/types/api";

interface TileCheckBadgeProps {
  summary: CheckSummary | null | undefined;
  queryId: number;
  /** Whether the viewer may create a monitor for this query (published query + permission). */
  canAddMonitor: boolean;
}

const PILL = "inline-flex shrink-0 items-center rounded-full border px-2 py-0.5 text-xs font-medium whitespace-nowrap";

const plural = (n: number, one: string, many: string) => `${n} ${n === 1 ? one : many}`;

/** The tile footer's check status pill, worst state first; links to the query's monitors. */
export function TileCheckBadge({ summary, queryId, canAddMonitor }: TileCheckBadgeProps) {
  const s = summary && summary.total > 0 ? summary : null;
  let text: string;
  let tone: string;
  if (!s) {
    if (!canAddMonitor) {
      return <span className={`${PILL} ml-auto border-transparent text-muted-foreground`}>No checks</span>;
    }
    return (
      <Link
        to={`/monitors/new?query_id=${queryId}`}
        className={`${PILL} ml-auto border-dashed border-border text-muted-foreground hover:bg-muted`}
      >
        No checks · add one
      </Link>
    );
  } else if (s.triggered > 0) {
    text = `${plural(s.triggered, "check", "checks")} failing`;
    tone = "border-destructive/30 bg-destructive/10 text-destructive";
  } else if (s.unevaluable > 0) {
    text = `${plural(s.unevaluable, "check", "checks")} not run`;
    tone = "border-warning/30 bg-warning/10 text-warning-foreground";
  } else if (s.ok > 0) {
    text = `${s.ok} ${s.ok === 1 ? "check passes" : "checks pass"}`;
    tone = "border-emerald-600/30 bg-emerald-600/10 text-emerald-700 dark:text-emerald-400";
  } else {
    text = `${plural(s.total, "check", "checks")} pending`;
    tone = "border-border bg-muted text-muted-foreground";
  }
  return (
    <Link to={`/monitors?query_id=${queryId}`} className={`${PILL} ml-auto hover:underline ${tone}`}>
      {text}
    </Link>
  );
}
