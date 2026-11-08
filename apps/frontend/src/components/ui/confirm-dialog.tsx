import type { ReactNode } from "react";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Button } from "@/components/ui/button";

interface ConfirmDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  title: string;
  description?: string;
  /** Extra content between the description and the footer — e.g. a bulleted list of
   * consequences too structured for the plain-text `description`. */
  body?: ReactNode;
  confirmLabel?: string;
  /** Confirm button text while `confirmPending`; defaults to `${confirmLabel}…`. */
  pendingLabel?: string;
  cancelLabel?: string;
  /** Keep Cancel enabled while `confirmPending`. */
  allowCancelWhilePending?: boolean;
  destructive?: boolean;
  onConfirm: () => void;
  confirmPending?: boolean;
  /** Failure message from a prior confirm attempt, e.g. a 409 from the mutation's onError.
   * Rendered inside the dialog — a sibling <p> in the page body sits behind the dialog overlay. */
  error?: string | null;
  testId?: string;
}

/** Generic confirm/cancel dialog for destructive or otherwise consequential actions. */
export function ConfirmDialog({
  open,
  onOpenChange,
  title,
  description,
  body,
  confirmLabel = "Confirm",
  pendingLabel = `${confirmLabel}…`,
  cancelLabel = "Cancel",
  allowCancelWhilePending = false,
  destructive = false,
  onConfirm,
  confirmPending = false,
  error,
  testId,
}: ConfirmDialogProps) {
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent data-testid={testId}>
        <DialogHeader>
          <DialogTitle>{title}</DialogTitle>
          {description && <DialogDescription>{description}</DialogDescription>}
        </DialogHeader>
        {body}
        {error && <p className="text-sm text-destructive">{error}</p>}
        <DialogFooter>
          <Button variant="outline" onClick={() => onOpenChange(false)} disabled={confirmPending && !allowCancelWhilePending}>
            {cancelLabel}
          </Button>
          <Button
            variant={destructive ? "destructive" : "default"}
            onClick={onConfirm}
            disabled={confirmPending}
          >
            {confirmPending ? pendingLabel : confirmLabel}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
