import { useEffect, useRef, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { AlertTriangle, Copy, Trash2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Loader } from "@/components/ui/loader";
import { DocsLink } from "@/components/common/DocsLink";
import { api, apiErrorMessage } from "@/lib/api";
import { copyToClipboard } from "@/lib/utils";
import { usePermissions } from "@/lib/permissions";
import type { Embed, EmbedObjectType, Visibility } from "@/types/api";

interface Props {
  objectType: EmbedObjectType;
  objectId: number;
  resourceTitle: string;
  /** For a visualization, its parent query's visibility (a viz has none of its own). */
  visibility: Visibility;
  onClose: () => void;
}

/** "Public link" dialog for a dashboard or a single visualization; the link and iframe snippet
 * share one token. */
export function ShareEmbedDialog({ objectType, objectId, resourceTitle, visibility, onClose }: Props) {
  const qc = useQueryClient();
  const { has } = usePermissions();
  const canCreateLink = has("create_embed");
  const canAnonymous = has("share_public_anonymous");
  const [confirmChecked, setConfirmChecked] = useState(false);
  const [anonymous, setAnonymous] = useState(true);
  const [error, setError] = useState<string | null>(null);
  // Whether the new link is reachable without signing in: false whenever the caller lacks
  // share_public_anonymous, regardless of the hidden checkbox.
  const willBeAnonymous = canAnonymous && anonymous;
  // Mirrors the backend gate (crates/shared/src/authz/access.rs::visibility_is_public); a link on a
  // restricted object would 403 immediately.
  const isPublicVisibility = visibility === "workspace" || visibility === "public";

  const embedsQuery = useQuery({
    queryKey: ["embeds", objectType, objectId],
    queryFn: () => api.listEmbeds(objectType, objectId),
  });

  const activeEmbeds = (embedsQuery.data ?? []).filter((e) => !e.revoked_at);

  const createMutation = useMutation({
    mutationFn: () =>
      api.createEmbed({
        object_type: objectType,
        object_id: objectId,
        anonymous: willBeAnonymous,
        // Every link publishes with no locked params, a static snapshot of the current cached
        // result.
        locked_params: {},
      }),
    onSuccess: async () => {
      setError(null);
      setConfirmChecked(false);
      await qc.invalidateQueries({ queryKey: ["embeds", objectType, objectId] });
    },
    onError: (e) => setError(apiErrorMessage(e, "Failed to create public link")),
  });

  const revokeMutation = useMutation({
    mutationFn: (id: number) => api.revokeEmbed(id),
    onSuccess: () => {
      setError(null);
      qc.invalidateQueries({ queryKey: ["embeds", objectType, objectId] });
    },
    onError: (e) => setError(apiErrorMessage(e, "Failed to revoke this link — it may still be live")),
  });

  const noun = objectType === "dashboard" ? "dashboard" : "visualization";
  const visibilityBlockedReason = isPublicVisibility
    ? undefined
    : `Set this ${noun}'s visibility to "Everyone" before creating a public link.`;

  // A plain render helper, not a nested component, so the checkboxes aren't remounted each render.
  const createForm = (hasLinks: boolean) => (
    <div className={`${hasLinks ? "space-y-2" : "space-y-3"} rounded-md border border-warning/40 bg-warning/10 p-3`}>
      {!hasLinks && (
        <>
          <p className="flex items-center gap-2 text-sm font-medium text-warning">
            <AlertTriangle className="size-4 shrink-0" />
            {willBeAnonymous
              ? "Anyone with the link can view this data without signing in."
              : "Anyone with the link can view this data, but must first sign in to your workspace."}
          </p>
          <p className="text-xs text-muted-foreground">
            Publishes a static snapshot as of the {noun}&rsquo;s last refresh. Viewers cannot change
            filters or run new queries. Revoke anytime.
            {willBeAnonymous &&
              " Some organizations require sign-in for every shared link regardless of this setting."}
          </p>
        </>
      )}
      {canAnonymous && (
        <label className="flex items-center gap-2 text-xs text-muted-foreground">
          <input type="checkbox" checked={anonymous} onChange={(e) => setAnonymous(e.target.checked)} />
          {hasLinks
            ? "New link: allow anyone with the link (no sign-in)"
            : "Allow anyone with the link (no sign-in). Unchecking still creates a link, but it requires the viewer to sign in to your workspace."}
        </label>
      )}
      <label className="flex items-center gap-2 text-xs">
        <input type="checkbox" checked={confirmChecked} onChange={(e) => setConfirmChecked(e.target.checked)} />
        {willBeAnonymous
          ? "I understand this data will be publicly accessible to anyone with the link."
          : "I understand this data will be accessible to anyone with the link who signs in to your workspace."}
      </label>
      {visibilityBlockedReason && <p className="text-xs text-destructive">{visibilityBlockedReason}</p>}
      <Button
        size="sm"
        variant={hasLinks ? "outline" : undefined}
        disabled={!confirmChecked || !isPublicVisibility || createMutation.isPending}
        title={visibilityBlockedReason}
        onClick={() => createMutation.mutate()}
      >
        {createMutation.isPending ? "Publishing…" : hasLinks ? "Create another public link" : "Create public link"}
      </Button>
    </div>
  );

  return (
    <Dialog open onOpenChange={onClose}>
      <DialogContent className="max-w-lg">
        <DialogHeader>
          <DialogTitle>Public link &mdash; {resourceTitle}</DialogTitle>
        </DialogHeader>
        <DocsLink topic="publicLinks" label="public links and embeds" />

        <div className="min-w-0 space-y-4">
          {error && <p className="text-sm text-destructive">{error}</p>}

          {embedsQuery.isPending ? (
            <div className="flex items-center gap-2 text-sm text-muted-foreground">
              <Loader size={16} announce /> Checking for an existing public link&hellip;
            </div>
          ) : embedsQuery.isError ? (
            <p className="text-sm text-destructive">
              {apiErrorMessage(embedsQuery.error, `Failed to load this ${noun}'s public links`)}
            </p>
          ) : activeEmbeds.length === 0 ? (
            <div className="space-y-3">
              <p className="text-sm text-muted-foreground">
                No public link. Only people in your workspace can see this {noun}.
              </p>

              {canCreateLink ? (
                createForm(false)
              ) : (
                <p className="text-xs text-muted-foreground">
                  Ask a workspace admin to create a public link for this {noun}.
                </p>
              )}
            </div>
          ) : (
            <div className="space-y-3">
              <p className="text-sm text-muted-foreground">
                This {noun} has {activeEmbeds.length} active public link
                {activeEmbeds.length > 1 ? "s" : ""}.
              </p>
              {activeEmbeds.map((embed) => (
                <ActiveEmbedRow
                  key={embed.id}
                  embed={embed}
                  onRevoke={() => revokeMutation.mutate(embed.id)}
                  revoking={revokeMutation.isPending && revokeMutation.variables === embed.id}
                />
              ))}

              {canCreateLink && createForm(true)}
            </div>
          )}
        </div>
      </DialogContent>
    </Dialog>
  );
}

function ActiveEmbedRow({
  embed,
  onRevoke,
  revoking,
}: {
  embed: Embed;
  onRevoke: () => void;
  revoking: boolean;
}) {
  const publicUrl = embed.public_url ?? "";
  const snippet = `<iframe src="${publicUrl}" width="800" height="600" frameborder="0"></iframe>`;
  const [copied, setCopied] = useState(false);
  const [copyFailed, setCopyFailed] = useState(false);
  const copyTimeout = useRef<ReturnType<typeof setTimeout> | null>(null);

  useEffect(() => () => { if (copyTimeout.current) clearTimeout(copyTimeout.current); }, []);

  async function handleCopy() {
    const ok = await copyToClipboard(publicUrl);
    setCopyFailed(!ok);
    if (!ok) return;
    setCopied(true);
    if (copyTimeout.current) clearTimeout(copyTimeout.current);
    copyTimeout.current = setTimeout(() => setCopied(false), 2000);
  }

  return (
    <div className="space-y-2 rounded-md border border-border p-3">
      <div className="flex gap-2">
        <Input readOnly value={publicUrl} className="font-mono text-xs" />
        <Button size="sm" variant="outline" onClick={handleCopy}>
          <Copy className="size-3.5" />
          {copied ? "Copied" : "Copy"}
        </Button>
      </div>
      {copyFailed && (
        <p className="text-xs text-destructive">
          Couldn&rsquo;t copy — select the link and copy it manually.
        </p>
      )}
      <pre className="overflow-x-auto rounded bg-muted p-2 text-xs">{snippet}</pre>
      <div className="flex items-center justify-between text-xs text-muted-foreground">
        <span>
          Created {new Date(embed.created_at).toLocaleString()}
          {embed.expires_at && ` · expires ${new Date(embed.expires_at).toLocaleDateString()}`}
          {!embed.anon_authorized && " · requires sign-in"}
        </span>
        <Button size="sm" variant="destructive" onClick={onRevoke} disabled={revoking}>
          <Trash2 className="size-3.5" />
          Revoke
        </Button>
      </div>
    </div>
  );
}
