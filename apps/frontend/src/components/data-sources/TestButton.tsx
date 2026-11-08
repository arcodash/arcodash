import { useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { CheckCircle, XCircle } from "lucide-react";
import { Button } from "@/components/ui/button";
import { apiErrorMessage, api } from "@/lib/api";

interface TestButtonProps {
  /** Test an unsaved connection by type+options, or an existing one by id. */
  target: { type: string; options: Record<string, unknown> } | { id: number };
  disabled?: boolean;
}

type TestState =
  | { status: "idle" }
  | { status: "testing" }
  | { status: "success"; message: string }
  | { status: "error"; message: string };

export function TestButton({ target, disabled }: TestButtonProps) {
  const [state, setState] = useState<TestState>({ status: "idle" });
  const queryClient = useQueryClient();

  async function handleTest() {
    setState({ status: "testing" });
    try {
      const res = await api.testDataSource(target);
      setState({ status: "success", message: res.message });
    } catch (err) {
      setState({ status: "error", message: apiErrorMessage(err, "Connection failed: could not reach the server") });
    } finally {
      // An explicit test writes health_state/health_error server-side (success or failure) —
      // refetch so the health badge and failing-state banner don't go stale until reload.
      if ("id" in target) {
        queryClient.invalidateQueries({ queryKey: ["data-source", target.id] });
        queryClient.invalidateQueries({ queryKey: ["data-sources"] });
      }
    }
  }

  return (
    <div className="flex items-center gap-3">
      <Button
        type="button"
        variant="outline"
        onClick={handleTest}
        disabled={disabled || state.status === "testing"}
      >
        {state.status === "testing" ? "Testing…" : "Test connection"}
      </Button>
      {"message" in state && (
        <span
          className={`flex items-center gap-1.5 text-sm ${state.status === "success" ? "text-green-600" : "text-destructive"}`}
        >
          {state.status === "success" ? (
            <CheckCircle className="w-4 h-4" aria-hidden="true" />
          ) : (
            <XCircle className="w-4 h-4" aria-hidden="true" />
          )}
          {state.message}
        </span>
      )}
    </div>
  );
}
