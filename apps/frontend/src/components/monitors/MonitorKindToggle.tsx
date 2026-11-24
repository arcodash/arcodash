import { Fragment } from "react";
import { Button } from "@/components/ui/button";
import type { MonitorKind } from "@/lib/monitors";

const KINDS: { value: MonitorKind; label: string }[] = [
  { value: "threshold", label: "Threshold" },
  { value: "baseline", label: "Baseline" },
];

export function MonitorKindToggle({
  value,
  onChange,
  disabled,
  labelId,
  suggested,
}: {
  value: MonitorKind;
  onChange: (kind: MonitorKind) => void;
  disabled?: boolean;
  labelId: string;
  /** Kind to flag with a "Suggested" label. */
  suggested?: MonitorKind;
}) {
  return (
    <div role="group" aria-labelledby={labelId} className="mt-1 flex gap-2">
      {KINDS.map((k) => (
        <Fragment key={k.value}>
        <Button
          type="button"
          size="sm"
          variant={value === k.value ? "default" : "outline"}
          aria-pressed={value === k.value}
          aria-describedby={suggested === k.value ? `${labelId}-suggested` : undefined}
          disabled={disabled}
          onClick={() => onChange(k.value)}
        >
          {k.label}
        </Button>
        {suggested === k.value && (
          <span id={`${labelId}-suggested`} className="self-center text-xs text-emerald-700 dark:text-emerald-400">
            Suggested for this tile
          </span>
        )}
        </Fragment>
      ))}
    </div>
  );
}
