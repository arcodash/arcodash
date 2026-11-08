import { Badge } from "@/components/ui/badge";
import type { AiProviderInfo, TestKeyResult } from "@/types/api";

const STATUS_LABEL: Record<string, string> = {
  ok: "OK",
  invalid_key: "Invalid key",
  quota_exceeded: "Quota exceeded",
  error: "Failed",
  unavailable: "Busy",
};

const BUSY_HINT =
  "The provider reported high demand on its last request. This is usually temporary — try again shortly.";

// warning's foreground is near-black (reads fine on its own light tint); success/destructive use
// their own base color as text since their "-foreground" pair is near-white, made for solid fills.
// Also styles the Test-key result, whose statuses overlap these.
export const STATUS_CLASS: Record<NonNullable<AiProviderInfo["last_status"]> | TestKeyResult["status"], string> = {
  ok: "border-success/30 bg-success/10 text-success",
  invalid_key: "border-destructive/30 bg-destructive/10 text-destructive",
  error: "border-destructive/30 bg-destructive/10 text-destructive",
  network_error: "border-destructive/30 bg-destructive/10 text-destructive",
  quota_exceeded: "border-warning/30 bg-warning/10 text-warning-foreground",
  unavailable: "border-warning/30 bg-warning/10 text-warning-foreground",
};

/** Shared health pill for a provider's most recent test/generation outcome — used on provider
 * cards and the priority list. Renders nothing until a real call has happened. */
export function ProviderStatusBadge({ status }: { status: AiProviderInfo["last_status"] }) {
  if (!status) return null;
  return (
    <Badge variant="outline" className={STATUS_CLASS[status]}
      title={status === "unavailable" ? BUSY_HINT : undefined}
    >
      {STATUS_LABEL[status]}
    </Badge>
  );
}
