import { useState } from "react";
import { CheckCircle, XCircle } from "lucide-react";
import { Button } from "@/components/ui/button";
import { ApiError, api } from "@/lib/api";

interface DestinationTestButtonProps {
  /** Test an unsaved configuration by type+options, or an existing one by id. */
  target: { type: string; options: Record<string, unknown> } | { id: number };
  destinationType?: string;
  disabled?: boolean;
}

type TestState =
  | { status: "idle" }
  | { status: "testing" }
  | { status: "success"; message: string }
  | { status: "error"; message: string };

/** "Send test message": nothing is "connected" here (a message was delivered), so the success copy
 * avoids connection wording. */
export function DestinationTestButton({
  target,
  destinationType,
  disabled,
}: DestinationTestButtonProps) {
  const [state, setState] = useState<TestState>({ status: "idle" });
  const kind = "type" in target ? target.type : destinationType;

  async function handleTest() {
    setState({ status: "testing" });
    try {
      await api.testDestination(target);
      setState({ status: "success", message: "Test message sent" });
    } catch (err) {
      setState({
        status: "error",
        message: err instanceof ApiError ? err.message : "Failed to send test message",
      });
    }
  }

  return (
    <div className="space-y-1">
      <div className="flex items-center gap-3">
        <Button
          type="button"
          variant="outline"
          onClick={handleTest}
          disabled={disabled || state.status === "testing"}
        >
          {state.status === "testing" ? "Sending…" : "Send test message"}
        </Button>
        {state.status === "success" && (
          <span className="flex items-center gap-1.5 text-sm text-success">
            <CheckCircle className="w-4 h-4" aria-hidden="true" />
            {state.message}
          </span>
        )}
        {state.status === "error" && (
          <span className="flex items-center gap-1.5 text-sm text-destructive">
            <XCircle className="w-4 h-4" aria-hidden="true" />
            {state.message}
          </span>
        )}
      </div>
      {kind === "pagerduty" && (
        <p className="text-xs text-muted-foreground">
          Opens and immediately resolves a test incident.
        </p>
      )}
    </div>
  );
}
