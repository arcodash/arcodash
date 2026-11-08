import { useEffect, useMemo, useRef, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Pencil, Plus, Share2, Trash2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Dialog, DialogContent, DialogFooter, DialogHeader, DialogTitle, DialogTrigger } from "@/components/ui/dialog";
import { ConfirmDialog } from "@/components/ui/confirm-dialog";
import { ResultsTable } from "@/components/query-editor/ResultsTable";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { api, apiErrorMessage } from "@/lib/api";
import { ErrorBoundary } from "@/components/ErrorBoundary";
import { ShareEmbedDialog } from "@/components/share/ShareEmbedDialog";
import type { ResultData, Visualization, Visibility } from "@/types/api";
import { vizRegistry, getViz, toVizData } from "./registry";
import type { QueryResultData } from "./registry";
import { toResultData } from "./table";

const tabClass = (active: boolean) =>
  `cursor-pointer rounded-t px-3 py-1 text-sm font-medium whitespace-nowrap transition-colors ${
    active ? "border-b-2 border-primary text-foreground" : "text-muted-foreground hover:text-foreground"
  }`;
const WARNING_CLASS =
  "rounded-md border border-warning/40 bg-warning/10 px-3 py-1.5 text-xs text-warning-foreground";

interface VizPanelProps {
  queryId: number;
  result: ResultData;
  /** A visualization inherits its parent query's visibility (same as backend public-link
   * eligibility); undefined while loading. */
  queryVisibility?: Visibility;
  /** Create-and-focus a visualization without `AddVizDialog`; cleared via `onAutoCreateHandled`. */
  autoCreateRequest?: { type: string; name: string; options: Record<string, unknown> } | null;
  onAutoCreateHandled?: () => void;
}

export function VizPanel({
  queryId,
  result,
  queryVisibility,
  autoCreateRequest,
  onAutoCreateHandled,
}: VizPanelProps) {
  const queryClient = useQueryClient();
  const [activeTab, setActiveTab] = useState<"results" | number>("results");
  const [editVizId, setEditVizId] = useState<number | null>(null);
  const [editOptions, setEditOptions] = useState<Record<string, unknown>>({});
  const [editName, setEditName] = useState("");
  const [editOpen, setEditOpen] = useState(false);
  const [confirmLeaveOpen, setConfirmLeaveOpen] = useState(false);
  const [shareVizId, setShareVizId] = useState<number | null>(null);
  const [vizError, setVizError] = useState<string | null>(null);
  const [pendingDeleteVizId, setPendingDeleteVizId] = useState<number | null>(null);

  const vizsQuery = useQuery({
    queryKey: ["visualizations", queryId],
    queryFn: () => api.listVisualizations(queryId),
  });

  // Editing the SQL never auto-revokes an embed, so this warning is informational only.
  const activeEmbedsQuery = useQuery({
    queryKey: ["embeds-active-count", queryId],
    queryFn: () => api.countActivePublicEmbeds(queryId),
  });

  const vizData = useMemo(() => toVizData(result), [result]);
  const vizs: Visualization[] = vizsQuery.data ?? [];

  const createMutation = useMutation({
    mutationFn: (body: { type: string; name: string; options?: Record<string, unknown> }) =>
      api.createVisualization({
        query_id: queryId,
        type: body.type,
        name: body.name,
        options: body.options ?? getViz(body.type)?.defaultOptions ?? {},
      }),
    onSuccess: async (viz) => {
      setVizError(null);
      await queryClient.invalidateQueries({ queryKey: ["visualizations", queryId] });
      setActiveTab(viz.id);
    },
    onError: (err) => setVizError(apiErrorMessage(err, "Failed to create visualization")),
  });

  // Runs a caller-supplied create request once per object identity, so re-renders never re-fire the
  // mutation.
  const handledAutoCreateRequest = useRef<typeof autoCreateRequest>(null);
  useEffect(() => {
    if (autoCreateRequest && handledAutoCreateRequest.current !== autoCreateRequest) {
      handledAutoCreateRequest.current = autoCreateRequest;
      createMutation.mutate(autoCreateRequest);
      onAutoCreateHandled?.();
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [autoCreateRequest]);

  // What the editor opened with, so Save sends only touched keys: the server replaces `options`
  // wholesale, so echoing the snapshot would revert a concurrent editor's changes.
  const editBaseRef = useRef<Record<string, unknown>>({});

  const updateMutation = useMutation({
    mutationFn: async ({
      id,
      options,
      name,
    }: {
      id: number;
      options: Record<string, unknown>;
      name?: string;
    }) => {
      const changed = Object.fromEntries(
        Object.entries(options).filter(
          ([k, v]) => JSON.stringify(v) !== JSON.stringify(editBaseRef.current[k]),
        ),
      );
      // The app-wide 30s staleTime would serve the cached row the dialog opened with, defeating the
      // re-read before merging.
      const fresh = await queryClient.fetchQuery({
        queryKey: ["visualizations", queryId],
        queryFn: () => api.listVisualizations(queryId),
        staleTime: 0,
      });
      const current = fresh.find((v) => v.id === id);
      return api.updateVisualization(id, {
        ...(name !== undefined && { name }),
        options: { ...current?.options, ...changed },
      });
    },
    onSuccess: async () => {
      setVizError(null);
      await queryClient.invalidateQueries({ queryKey: ["visualizations", queryId] });
      setEditOpen(false);
    },
    onError: (err) => setVizError(apiErrorMessage(err, "Failed to save visualization")),
  });

  const deleteMutation = useMutation({
    mutationFn: (id: number) => api.deleteVisualization(id),
    onSuccess: async (_, id) => {
      setVizError(null);
      setPendingDeleteVizId(null);
      await queryClient.invalidateQueries({ queryKey: ["visualizations", queryId] });
      if (activeTab === id) setActiveTab("results");
    },
    onError: (err) => setVizError(apiErrorMessage(err, "Failed to delete visualization")),
  });

  function openEditor(viz: Visualization) {
    setEditVizId(viz.id);
    setEditOptions({ ...viz.options });
    setEditName(viz.name);
    editBaseRef.current = { ...viz.options };
    setEditOpen(true);
  }

  const activeViz = vizs.find((v) => v.id === activeTab);
  const vizDef = activeViz ? getViz(activeViz.type) : null;
  const editViz = vizs.find((v) => v.id === editVizId);
  const editDef = editViz ? getViz(editViz.type) : null;
  const shareViz = vizs.find((v) => v.id === shareVizId);
  const embedCount = activeEmbedsQuery.data?.count ?? 0;
  const renderer = activeViz && vizDef && <vizDef.Renderer data={vizData} options={activeViz.options} />;
  const trimmedName = editName.trim();

  function cancelEdit() {
    const dirty =
      JSON.stringify(editOptions) !== JSON.stringify(editViz?.options) || trimmedName !== editViz?.name;
    if (dirty) setConfirmLeaveOpen(true);
    else setEditOpen(false);
  }

  return (
    <div className="space-y-3">
      <div className="flex items-center gap-1 border-b border-border pb-1 overflow-x-auto custom-scroll">
        <button className={tabClass(activeTab === "results")} onClick={() => setActiveTab("results")}>
          Results
        </button>
        {vizs.map((viz) => (
          <div key={viz.id} className="flex items-center">
            <button className={tabClass(activeTab === viz.id)} onClick={() => setActiveTab(viz.id)}>
              {viz.name}
            </button>
            {activeTab === viz.id && (
              <>
                <button
                  className="ml-1 cursor-pointer rounded p-0.5 text-muted-foreground hover:text-foreground"
                  title="Public link"
                  onClick={() => setShareVizId(viz.id)}
                >
                  <Share2 className="size-3" />
                </button>
                <button
                  className="ml-1 cursor-pointer rounded p-0.5 text-muted-foreground hover:text-foreground"
                  title="Edit visualization"
                  onClick={() => openEditor(viz)}
                >
                  <Pencil className="size-3" />
                </button>
                <button
                  className="ml-0.5 cursor-pointer rounded p-0.5 text-muted-foreground hover:text-destructive"
                  title="Delete visualization"
                  onClick={() => { setVizError(null); setPendingDeleteVizId(viz.id); }}
                  disabled={deleteMutation.isPending}
                >
                  <Trash2 className="size-3" />
                </button>
              </>
            )}
          </div>
        ))}
        <AddVizDialog onCreate={(type, name) => createMutation.mutateAsync({ type, name })} />
      </div>

      {vizError && <p className="text-sm text-destructive">{vizError}</p>}

      {activeEmbedsQuery.isError ? (
        <p className={WARNING_CLASS}>
          Couldn&apos;t check whether this query has active public embeds. If it does, editing the
          SQL changes what they show.
        </p>
      ) : embedCount > 0 ? (
        <p className={WARNING_CLASS}>
          This query has {embedCount} active public embed
          {embedCount > 1 ? "s" : ""}. Editing the SQL will not revoke them —
          published snapshots keep showing old data until the query refreshes.
        </p>
      ) : null}

      <div>
        {activeTab === "results" && <DefaultTable vizData={vizData} />}
        {activeViz && vizDef && (
          <ErrorBoundary>
            {/* Chart's ResponsiveContainer resolves height="100%" against a sized ancestor; other renderers size themselves. */}
            {vizDef.type === "CHART" ? (
              <div className="h-[400px]">{renderer}</div>
            ) : (
              renderer
            )}
          </ErrorBoundary>
        )}
        {activeViz && !vizDef && (
          <div className="p-4 text-sm text-muted-foreground">
            Unknown visualization type &ldquo;{activeViz.type}&rdquo;.
          </div>
        )}
      </div>

      {editViz && editDef && (
        <>
          <Dialog open={editOpen} onOpenChange={setEditOpen}>
            <DialogContent
              className="max-w-lg max-h-[80vh] overflow-y-auto"
              showCloseButton={false}
              onEscapeKeyDown={(e) => e.preventDefault()}
              onInteractOutside={(e) => e.preventDefault()}
            >
              <DialogHeader>
                <DialogTitle>Edit {editViz.name}</DialogTitle>
              </DialogHeader>
              <div className="space-y-1">
                <Label htmlFor="edit-viz-name">
                  Name<span className="text-destructive"> *</span>
                </Label>
                <Input
                  id="edit-viz-name"
                  value={editName}
                  onChange={(e) => setEditName(e.target.value)}
                />
              </div>
              <editDef.Editor data={vizData} options={editOptions} onChange={setEditOptions} />
              <DialogFooter>
                <Button variant="outline" onClick={cancelEdit}>
                  Cancel
                </Button>
                <Button
                  onClick={() =>
                    updateMutation.mutate({
                      id: editViz.id,
                      options: editOptions,
                      name: trimmedName !== editViz.name ? trimmedName : undefined,
                    })
                  }
                  disabled={updateMutation.isPending || !trimmedName}
                >
                  {updateMutation.isPending ? "Saving…" : "Save"}
                </Button>
              </DialogFooter>
            </DialogContent>
          </Dialog>
          <ConfirmDialog
            open={confirmLeaveOpen}
            onOpenChange={setConfirmLeaveOpen}
            title="Discard unsaved changes?"
            description="Your latest edits haven't saved yet. Leave anyway?"
            confirmLabel="Leave"
            destructive
            onConfirm={() => {
              setConfirmLeaveOpen(false);
              setEditOpen(false);
            }}
          />
        </>
      )}

      <ConfirmDialog
        open={pendingDeleteVizId != null}
        onOpenChange={(o) => { if (!o) { setPendingDeleteVizId(null); setVizError(null); } }}
        title="Delete this visualization?"
        description="Any dashboard widgets using it will also be removed."
        confirmLabel="Delete"
        destructive
        confirmPending={deleteMutation.isPending}
        error={vizError}
        onConfirm={() => { if (pendingDeleteVizId != null) deleteMutation.mutate(pendingDeleteVizId); }}
      />

      {shareViz && (
        <ShareEmbedDialog
          objectType="visualization"
          objectId={shareViz.id}
          resourceTitle={shareViz.name}
          visibility={queryVisibility ?? "restricted"}
          onClose={() => setShareVizId(null)}
        />
      )}
    </div>
  );
}

function DefaultTable({ vizData: { columns, rows } }: { vizData: QueryResultData }) {
  const resultData = useMemo(() => toResultData(columns, rows), [columns, rows]);
  return <ResultsTable data={resultData} />;
}

function AddVizDialog({ onCreate }: { onCreate: (type: string, name: string) => Promise<unknown> }) {
  const [open, setOpen] = useState(false);
  const [type, setType] = useState("TABLE");
  const [name, setName] = useState("");
  const [creating, setCreating] = useState(false);
  const [createError, setCreateError] = useState<string | null>(null);

  async function handleCreate() {
    if (!name) return;
    setCreating(true);
    setCreateError(null);
    try {
      await onCreate(type, name);
      setOpen(false);
      setName("");
    } catch (err) {
      // Keep the dialog open and the typed name; the parent's error banner is hidden behind this
      // modal.
      setCreateError(apiErrorMessage(err, "Failed to create visualization"));
    } finally {
      setCreating(false);
    }
  }

  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogTrigger asChild>
        <Button variant="outline" size="sm" className="ml-2 shrink-0">
          <Plus className="size-3.5" />
          Add Visualization
        </Button>
      </DialogTrigger>
      <DialogContent className="max-w-sm">
        <DialogHeader>
          <DialogTitle>Add Visualization</DialogTitle>
        </DialogHeader>
        <div className="space-y-4">
          <div className="space-y-1">
            <Label>Type</Label>
            <Select value={type} onValueChange={setType}>
              <SelectTrigger>
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                {vizRegistry.map((def) => (
                  <SelectItem key={def.type} value={def.type}>
                    {def.name}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </div>
          <div className="space-y-1">
            <Label>
              Name<span className="text-destructive"> *</span>
            </Label>
            <Input
              placeholder="My visualization"
              value={name}
              onChange={(e) => setName(e.target.value)}
              onKeyDown={(e) => e.key === "Enter" && handleCreate()}
            />
          </div>
          {createError && <p className="text-sm text-destructive">{createError}</p>}
        </div>
        <DialogFooter>
          <Button variant="outline" onClick={() => setOpen(false)} disabled={creating}>
            Cancel
          </Button>
          <Button
            onClick={handleCreate}
            disabled={!name || creating}
            title={!name ? "Enter a name" : undefined}
          >
            {creating ? "Creating…" : "Add"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
