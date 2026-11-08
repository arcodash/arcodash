import { Badge } from "@/components/ui/badge";
import { cn } from "@/lib/utils";

// One vocabulary per state, used everywhere a monitor's state is shown (list, detail, email,
// audit log) — "Alerting" over "Triggered" because the state is a condition that IS currently
// true, not a past event that already fired and is over.
export function StateBadge({ state, checkFailed }: { state: string; checkFailed?: boolean }) {
  if (state === "triggered") return <Badge variant="destructive">Alerting</Badge>;
  if (checkFailed) {
    return (
      <Badge variant="outline" className="border-destructive/50 bg-destructive/10 text-destructive">
        Check failed
      </Badge>
    );
  }
  if (state === "ok") return <Badge className="bg-green-100 text-green-800">OK</Badge>;
  // Distinct from "unknown" (never checked yet) — there IS a result, the condition just
  // couldn't be computed against it (missing column, non-numeric value, ...).
  if (state === "unevaluable") {
    return <Badge className="bg-warning/20 text-warning-foreground">Can't evaluate</Badge>;
  }
  return <Badge variant="outline">Not checked yet</Badge>;
}

/** A published query/dashboard whose saved edits aren't published yet. */
export function UnpublishedBadge({ className }: { className?: string }) {
  return (
    <Badge variant="outline" className={cn("gap-1.5 border-warning/30 bg-warning/10 text-warning-foreground", className)}>
      <span className="size-1.5 rounded-full bg-warning" />
      Unpublished changes
    </Badge>
  );
}
