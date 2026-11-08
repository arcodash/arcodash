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
}: {
  value: MonitorKind;
  onChange: (kind: MonitorKind) => void;
  disabled?: boolean;
  labelId: string;
}) {
  return (
    <div role="group" aria-labelledby={labelId} className="mt-1 flex gap-2">
      {KINDS.map((k) => (
        <Button
          key={k.value}
          type="button"
          size="sm"
          variant={value === k.value ? "default" : "outline"}
          aria-pressed={value === k.value}
          disabled={disabled}
          onClick={() => onChange(k.value)}
        >
          {k.label}
        </Button>
      ))}
    </div>
  );
}
