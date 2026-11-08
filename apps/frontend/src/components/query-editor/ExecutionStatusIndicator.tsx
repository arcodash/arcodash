import { AlertCircle, CheckCircle, XCircle } from "lucide-react";
import type { Query } from "@/types/api";

/** Status -> icon + color, shared with any other view that needs the same signal at a glance. */
export function executionStatusIcon(status: Query["last_execution_status"]) {
  if (status === "Success") return { Icon: CheckCircle, colorClassName: "text-green-500" };
  if (status === "Failed") return { Icon: XCircle, colorClassName: "text-red-500" };
  return { Icon: AlertCircle, colorClassName: "text-muted-foreground" };
}

export function ExecutionStatusIndicator({ status }: { status: Query["last_execution_status"] }) {
  const { Icon, colorClassName } = executionStatusIcon(status);
  return (
    <span className={`flex items-center gap-1.5 text-sm ${status === "New" ? "text-muted-foreground" : ""}`}>
      <Icon className={`w-4 h-4 ${colorClassName}`} /> {status}
    </span>
  );
}
