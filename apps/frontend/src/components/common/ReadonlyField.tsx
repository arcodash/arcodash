import type { ReactNode } from "react";
import { Label } from "@/components/ui/label";

/** A stored value the user can't edit here — not an `<input disabled>`, whose
 * `disabled:opacity-50` renders real text at the same weight as a placeholder and makes a real
 * value (e.g. an email address) look empty. Full-opacity text on a muted background instead. */
export function ReadonlyField({
  label,
  value,
  hint,
  id,
}: {
  label: string;
  value: ReactNode;
  hint?: string;
  id?: string;
}) {
  return (
    <div className="space-y-1">
      <Label id={id ? `${id}-label` : undefined}>{label}</Label>
      <p
        id={id}
        aria-labelledby={id ? `${id}-label` : undefined}
        className="min-h-10 rounded-md border border-border bg-muted/40 px-3 py-[9px] text-sm [overflow-wrap:anywhere] text-foreground"
      >
        {value}
      </p>
      {hint && <p className="text-xs text-muted-foreground">{hint}</p>}
    </div>
  );
}
