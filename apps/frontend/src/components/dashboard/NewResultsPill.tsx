import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";
import { newResultsLabel } from "@/lib/newResults";

/** Announces tiles with newer results; the live region stays mounted so updates are read out. */
export function NewResultsPill({ count, onShow }: { count: number; onShow: () => void }) {
  return (
    <div
      className={cn(
        "flex items-center gap-2",
        count > 0 && "rounded-md bg-warning/10 px-2.5 py-1 text-xs text-warning-foreground",
      )}
    >
      <span role="status" aria-live="polite">
        {count > 0 ? newResultsLabel(count) : ""}
      </span>
      {count > 0 && (
        <Button
          variant="outline"
          size="sm"
          aria-label="Show new results"
          className="h-6 border-warning bg-background px-2 text-xs"
          onClick={onShow}
        >
          Show
        </Button>
      )}
    </div>
  );
}
