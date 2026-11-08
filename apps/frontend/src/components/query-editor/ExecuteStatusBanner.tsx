import type { ReactNode } from "react";

interface ExecuteStatusBannerProps {
  isPending: boolean;
  isStarted: boolean;
  isFailure: boolean;
  isCancelled: boolean;
  gaveUp: boolean;
  error: string | null;
}

export function ExecuteStatusBanner({
  isPending,
  isStarted,
  isFailure,
  isCancelled,
  gaveUp,
  error,
}: ExecuteStatusBannerProps) {
  let content: ReactNode;
  if (gaveUp) {
    content = error;
  } else if (isPending || isStarted) {
    content = isPending ? "Query queued…" : "Running query…";
  } else if (isFailure) {
    return (
      <div
        role="status"
        className="rounded-md border border-destructive/30 bg-destructive/5 p-3 text-sm text-destructive"
      >
        {error ?? "Query execution failed."}
      </div>
    );
  } else if (isCancelled) {
    // A scheduled run skipped for a real reason (e.g. its data source is paused) carries that
    // reason in `error` — showing it beats the generic copy below, which wrongly implies the
    // source database might still be executing the query.
    content = error ? (
      <p>{error}</p>
    ) : (
      <>
        <p>Query stopped. No results were saved.</p>
        <p className="mt-1 text-xs">
          The source database may keep running it until it finishes or hits its own statement timeout.
        </p>
      </>
    );
  } else {
    return null;
  }

  return (
    <div role="status" className="rounded-md border border-border bg-muted p-3 text-sm text-muted-foreground">
      {content}
    </div>
  );
}
