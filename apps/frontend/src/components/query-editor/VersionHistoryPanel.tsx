import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { History, RotateCcw } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Dialog, DialogContent, DialogTitle } from "@/components/ui/dialog";
import { Loader } from "@/components/ui/loader";
import { DocsLink } from "@/components/common/DocsLink";
import { ConflictError, api, apiErrorMessage } from "@/lib/api";
import type { QueryVersion } from "@/types/api";

interface VersionHistoryPanelProps {
  queryId: number;
  workingVersion: number;
  /** Restore always republishes; a currently-unpublished query needs that called out since it
   * silently goes live. */
  isCurrentlyPublished: boolean;
  /** Called after a revert/restore once the panel has closed, so the parent can re-hydrate and
   * surface `message`. */
  onReverted: (message: string) => void;
  /** A stale `workingVersion` (409) — the parent owns the reload banner, not this panel. */
  onConflict: (err: ConflictError) => void;
  open: boolean;
  onOpenChange: (open: boolean) => void;
}

const DIFFABLE_FIELDS: Array<{ key: keyof QueryVersion["change"]; label: string }> = [
  { key: "name", label: "Name" },
  { key: "description", label: "Description" },
  { key: "query", label: "Query" },
  { key: "data_source_id", label: "Data source" },
  { key: "is_archived", label: "Archived" },
];

function formatValue(v: unknown): string {
  if (v == null) return "(none)";
  if (typeof v === "string" && v.length > 80) return `${v.slice(0, 80)}…`;
  return String(v);
}

/** Version history dropdown, newest first (the API is oldest-first): who/when, a revert action, and
 * a before/after diff against the previous version. */
export function VersionHistoryPanel({
  queryId,
  workingVersion,
  isCurrentlyPublished,
  onReverted,
  onConflict,
  open,
  onOpenChange,
}: VersionHistoryPanelProps) {
  const [error, setError] = useState<string | null>(null);
  const queryClient = useQueryClient();

  const versionsQuery = useQuery({
    queryKey: ["query-versions", queryId],
    queryFn: () => api.getQueryVersions(queryId),
    enabled: open,
  });

  async function finish(message: string) {
    setError(null);
    await Promise.all([
      queryClient.invalidateQueries({ queryKey: ["query-versions", queryId] }),
      queryClient.invalidateQueries({ queryKey: ["query", queryId] }),
    ]);
    onOpenChange(false);
    onReverted(message);
  }

  const revertMutation = useMutation({
    mutationFn: (changeId: number) => api.revertQuery(queryId, { change_id: changeId }),
    onSuccess: () => finish("Loaded into draft — publish to make it live."),
    onError: (err) => setError(apiErrorMessage(err, "Failed to revert")),
  });

  const restoreMutation = useMutation({
    mutationFn: (changeId: number) => api.restoreQueryVersion(queryId, changeId, workingVersion),
    onSuccess: () => finish("Restored and published."),
    onError: (err) => {
      if (err instanceof ConflictError) {
        onConflict(err);
        return;
      }
      setError(apiErrorMessage(err, "Failed to restore this version"));
    },
  });

  const versions = versionsQuery.data ?? [];
  const newestFirst = [...versions].reverse();
  const busy = revertMutation.isPending || restoreMutation.isPending;

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-w-md">
        <DialogTitle>Version history</DialogTitle>

        <p className="-mt-2 px-1 text-xs text-muted-foreground">
          Only published versions are kept — draft edits aren&apos;t recorded.{" "}
          <DocsLink topic="versionHistory" label="version history" />
        </p>
        {versions.length > 0 && (
          <p className="px-1 text-xs text-muted-foreground">{versions.length} version(s)</p>
        )}

        {error && <p className="mb-2 px-1 text-xs text-destructive">{error}</p>}

        <div className="max-h-96 space-y-2 overflow-auto">
          {versionsQuery.isLoading && (
            <div className="px-1 py-1.5">
              <Loader size={16} announce />
            </div>
          )}
          {!versionsQuery.isLoading && versions.length === 0 && (
            <p className="px-1 py-1.5 text-sm text-muted-foreground">No history yet.</p>
          )}
          {newestFirst.map((v, idx) => {
            // The previous version (one position later in this reversed list) is the diff baseline.
            const previous = newestFirst[idx + 1];
            const isLatest = idx === 0;
            return (
              <div key={v.change_id} className="rounded-md border border-border p-2 text-sm">
                <div className="flex items-center justify-between gap-2">
                  <div className="min-w-0">
                    <p className="font-medium">
                      Version {v.version}
                      {isLatest && (
                        <span className="ml-1.5 text-xs font-normal text-muted-foreground">
                          (current)
                        </span>
                      )}
                    </p>
                    <p className="truncate text-xs text-muted-foreground">
                      {v.user_name ?? `User #${v.user_id}`} · {new Date(v.created_at).toLocaleString()}
                    </p>
                  </div>
                  {!isLatest && (
                    <div className="flex shrink-0 items-center gap-1">
                      <Button
                        variant="ghost"
                        size="icon-sm"
                        title="Load into the draft (doesn't change what's published)"
                        onClick={() => revertMutation.mutate(v.change_id)}
                        disabled={busy}
                      >
                        <RotateCcw className="size-3.5" />
                      </Button>
                      <Button
                        variant="ghost"
                        size="icon-sm"
                        title={
                          isCurrentlyPublished
                            ? "Restore — make this version the published version"
                            : "Restore — publishes this version, even though the query is currently unpublished"
                        }
                        onClick={() => restoreMutation.mutate(v.change_id)}
                        disabled={busy}
                      >
                        <History className="size-3.5" />
                      </Button>
                    </div>
                  )}
                </div>

                {previous && <VersionDiff before={previous.change} after={v.change} />}
              </div>
            );
          })}
        </div>
      </DialogContent>
    </Dialog>
  );
}

function VersionDiff({ before, after }: { before: QueryVersion["change"]; after: QueryVersion["change"] }) {
  const changedFields = DIFFABLE_FIELDS.filter(({ key }) => before[key] !== after[key]);
  if (changedFields.length === 0) return null;

  return (
    <div className="mt-1.5 space-y-1 border-t border-border pt-1.5">
      {changedFields.map(({ key, label }) => (
        <div key={key} className="text-xs">
          <span className="font-medium text-muted-foreground">{label}: </span>
          <span className="text-destructive line-through">{formatValue(before[key])}</span>
          {" -> "}
          <span className="text-foreground">{formatValue(after[key])}</span>
        </div>
      ))}
    </div>
  );
}
