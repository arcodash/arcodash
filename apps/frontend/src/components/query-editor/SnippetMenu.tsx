import { useEffect, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Code2, Pencil, Plus, Trash2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Dialog, DialogContent, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import { Loader } from "@/components/ui/loader";
import { api, apiErrorMessage } from "@/lib/api";
import { usePermissions } from "@/lib/permissions";
import type { Snippet } from "@/types/api";

interface SnippetMenuProps {
  /** Called with the snippet's raw `${1:placeholder}` body and trigger word; the caller expands tab stops via
   * `SqlEditorHandle.insertSnippet`, replacing a typed trigger right before the cursor. */
  onInsert: (body: string, trigger: string) => void;
}

/** Snippet dropdown plus a "Manage snippets" modal (deliberately minimal, no dedicated route). */
export function SnippetMenu({ onInsert }: SnippetMenuProps) {
  const { has } = usePermissions();
  const [open, setOpen] = useState(false);
  const [manageOpen, setManageOpen] = useState(false);

  const snippetsQuery = useQuery({ queryKey: ["snippets"], queryFn: () => api.listSnippets() });
  const snippets = snippetsQuery.data ?? [];

  return (
    <>
      <Popover open={open} onOpenChange={setOpen}>
        <PopoverTrigger asChild>
          <Button variant="outline" size="sm">
            <Code2 className="size-3.5" />
            Snippets
          </Button>
        </PopoverTrigger>
        <PopoverContent align="start" className="w-80 max-w-[calc(100vw-2rem)] p-1">
          <div className="max-h-72 overflow-auto">
            {snippetsQuery.isLoading && (
              <div className="px-2 py-1.5">
                <Loader size={16} announce />
              </div>
            )}
            {!snippetsQuery.isLoading && snippets.length === 0 && (
              <p className="px-2 py-1.5 text-sm text-muted-foreground">
                No snippets yet in this org.
              </p>
            )}
            {snippets.map((s) => (
              <button
                key={s.id}
                type="button"
                className="cursor-pointer flex w-full flex-col items-start gap-0.5 rounded-sm px-2 py-1.5 text-left text-sm hover:bg-accent"
                onClick={() => {
                  onInsert(s.snippet, s.trigger);
                  setOpen(false);
                }}
              >
                <span className="font-medium">{s.trigger}</span>
                {s.description && (
                  <span className="text-xs text-muted-foreground">{s.description}</span>
                )}
              </button>
            ))}
          </div>
          {has("manage_snippets") && (
            <div className="mt-1 border-t border-border pt-1">
              <button
                type="button"
                className="cursor-pointer flex w-full items-center gap-1.5 rounded-sm px-2 py-1.5 text-left text-sm text-muted-foreground hover:bg-accent hover:text-foreground"
                onClick={() => {
                  setOpen(false);
                  setManageOpen(true);
                }}
              >
                <Pencil className="size-3.5" />
                Manage snippets…
              </button>
            </div>
          )}
        </PopoverContent>
      </Popover>

      <ManageSnippetsDialog open={manageOpen} onOpenChange={setManageOpen} snippets={snippets} />
    </>
  );
}

function ManageSnippetsDialog({
  open,
  onOpenChange,
  snippets,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  snippets: Snippet[];
}) {
  const queryClient = useQueryClient();
  const [editing, setEditing] = useState<Snippet | "new" | null>(null);
  const [error, setError] = useState<string | null>(null);

  // Reset on close so the next open does not show a stale form or error.
  useEffect(() => {
    if (!open) {
      setEditing(null);
      setError(null);
    }
  }, [open]);

  const deleteMutation = useMutation({
    mutationFn: (id: number) => api.deleteSnippet(id),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ["snippets"] }),
    onError: (err) => setError(apiErrorMessage(err, "Failed to delete snippet")),
  });

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-w-xl">
        <DialogHeader>
          <DialogTitle>Manage snippets</DialogTitle>
        </DialogHeader>

        {error && <p className="text-sm text-destructive">{error}</p>}

        {editing ? (
          <SnippetForm
            initial={editing === "new" ? null : editing}
            onDone={() => setEditing(null)}
            onError={setError}
          />
        ) : (
          <div className="flex flex-col gap-2">
            <div className="max-h-80 space-y-1 overflow-auto">
              {snippets.length === 0 && (
                <p className="text-sm text-muted-foreground">No snippets yet.</p>
              )}
              {snippets.map((s) => (
                <div
                  key={s.id}
                  className="flex items-center justify-between gap-2 rounded-md border border-border px-3 py-2"
                >
                  <div className="min-w-0">
                    <p className="truncate text-sm font-medium">{s.trigger}</p>
                    {s.description && (
                      <p className="truncate text-xs text-muted-foreground">{s.description}</p>
                    )}
                  </div>
                  <div className="flex shrink-0 items-center gap-1">
                    <Button
                      variant="ghost"
                      size="icon-sm"
                      aria-label={`Edit snippet ${s.trigger}`}
                      onClick={() => setEditing(s)}
                    >
                      <Pencil className="size-3.5" />
                    </Button>
                    <Button
                      variant="ghost"
                      size="icon-sm"
                      aria-label={`Delete snippet ${s.trigger}`}
                      onClick={() => deleteMutation.mutate(s.id)}
                      disabled={deleteMutation.isPending}
                    >
                      <Trash2 className="size-3.5" />
                    </Button>
                  </div>
                </div>
              ))}
            </div>
            <Button variant="outline" size="sm" className="self-start" onClick={() => setEditing("new")}>
              <Plus className="size-3.5" />
              New snippet
            </Button>
          </div>
        )}
      </DialogContent>
    </Dialog>
  );
}

function SnippetForm({
  initial,
  onDone,
  onError,
}: {
  initial: Snippet | null;
  onDone: () => void;
  onError: (message: string | null) => void;
}) {
  const queryClient = useQueryClient();
  const [trigger, setTrigger] = useState(initial?.trigger ?? "");
  const [description, setDescription] = useState(initial?.description ?? "");
  const [body, setBody] = useState(initial?.snippet ?? "");

  const saveMutation = useMutation({
    mutationFn: () => {
      const payload = { trigger, description, snippet: body };
      return initial ? api.updateSnippet(initial.id, payload) : api.createSnippet(payload);
    },
    onSuccess: async () => {
      onError(null);
      await queryClient.invalidateQueries({ queryKey: ["snippets"] });
      onDone();
    },
    onError: (err) => onError(apiErrorMessage(err, "Failed to save snippet")),
  });

  return (
    <form
      className="flex flex-col gap-3"
      onSubmit={(e) => {
        e.preventDefault();
        saveMutation.mutate();
      }}
    >
      <div className="flex flex-col gap-1">
        <Label htmlFor="snippet-trigger">Trigger</Label>
        <Input
          id="snippet-trigger"
          value={trigger}
          onChange={(e) => setTrigger(e.target.value)}
          placeholder="e.g. top10"
          required
        />
      </div>
      <div className="flex flex-col gap-1">
        <Label htmlFor="snippet-description">Description</Label>
        <Input
          id="snippet-description"
          value={description}
          onChange={(e) => setDescription(e.target.value)}
          placeholder="Optional"
        />
      </div>
      <div className="flex flex-col gap-1">
        <Label htmlFor="snippet-body">
          Snippet body — use <code className="text-xs">{"${1:placeholder}"}</code> tab-stops
        </Label>
        <textarea
          id="snippet-body"
          className="h-32 rounded-md border border-input bg-transparent px-3 py-2 font-mono text-sm"
          value={body}
          onChange={(e) => setBody(e.target.value)}
          placeholder={"SELECT ${1:column} FROM ${2:table} LIMIT ${3:100}"}
          required
        />
      </div>
      <DialogFooter>
        <Button type="button" variant="outline" onClick={onDone}>
          Cancel
        </Button>
        <Button type="submit" disabled={saveMutation.isPending}>
          {saveMutation.isPending ? "Saving…" : "Save"}
        </Button>
      </DialogFooter>
    </form>
  );
}
