import { PauseCircle } from "lucide-react";
import { Popover, PopoverClose, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import { formatAlertsAt, type LearningStatus } from "@/lib/monitorLearning";

export function LearningBadge({
  status,
  nowMs,
  canEdit,
  onChangeSchedule,
}: {
  status: LearningStatus;
  nowMs: number;
  canEdit: boolean;
  onChangeSchedule: () => void;
}) {
  const { have, need } = status;
  const paused = status.kind === "paused";
  return (
    <Popover>
      <PopoverTrigger asChild>
        <button
          type="button"
          data-testid="monitor-learning-badge"
          aria-label={status.ariaLabel}
          className="inline-flex items-center gap-1.5 rounded-full bg-secondary px-2.5 py-0.5 text-xs font-medium text-secondary-foreground hover:bg-secondary/80"
        >
          {paused ? (
            <PauseCircle className="size-3" aria-hidden />
          ) : (
            <span className="flex gap-0.5" aria-hidden>
              {Array.from({ length: need }, (_, i) => (
                <span key={i} className={`size-1.5 rounded-full ${i < have ? "bg-primary" : "bg-muted-foreground/40"}`} />
              ))}
            </span>
          )}
          {status.label}
        </button>
      </PopoverTrigger>
      <PopoverContent className="w-80 text-sm">
        <div className="space-y-3">
          <p className="font-medium">{paused ? "Learning is paused" : "Learning the usual range"}</p>
          <p className="text-muted-foreground">
            {paused
              ? "The check schedule is off, so no new checks run and this monitor can't alert."
              : `Baseline monitors alert when the value moves away from its recent median. They need ${need} checks first.`}
          </p>
          <div className="flex items-center gap-2">
            <div role="img" aria-label={`${have} of ${need} checks`} className="flex flex-1 gap-0.5">
              {Array.from({ length: need }, (_, i) => (
                <span key={i} className={`h-1.5 flex-1 rounded-full ${i < have ? "bg-primary" : "bg-muted"}`} />
              ))}
            </div>
            <span className="text-xs tabular-nums text-muted-foreground">
              {have} of {need} checks
            </span>
          </div>
          {!paused && (
            <p className="text-xs text-muted-foreground">
              {status.kind === "active" && status.alertsAtMs != null
                ? `${status.cadenceLabel} · alerts start around ${formatAlertsAt(status.alertsAtMs, nowMs)}`
                : `Alerts start after ${need} checks.`}
            </p>
          )}
          <PopoverClose asChild>
            <button type="button" className="text-xs underline underline-offset-2" onClick={onChangeSchedule}>
              {!canEdit ? "View schedule" : paused ? "Turn on schedule" : "Change schedule"}
            </button>
          </PopoverClose>
        </div>
      </PopoverContent>
    </Popover>
  );
}
