import { useEffect, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Lock, User, Users, X } from "lucide-react";
import { api, apiErrorMessage } from "@/lib/api";
import { matchesSearch } from "@/lib/utils";
import { usePermissions } from "@/lib/permissions";
import type { AccessPermission, ObjectRole, ObjectType, Visibility } from "@/types/api";
import {
  Dialog,
  DialogContent,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Loader } from "@/components/ui/loader";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { OwnerChip } from "./OwnerChip";

interface ShareDialogProps {
  objectType: ObjectType;
  objectId: number;
  ownerId: number;
  visibility: Visibility;
  open: boolean;
  canManage: boolean;
  onClose: () => void;
  onChanged?: () => void;
}

const VISIBILITY_HELP: Record<"workspace" | "restricted", string> = {
  workspace: "Everyone in your workspace can view.",
  restricted: "Only the people and groups you add below can view.",
};

/** Access-control dialog for a query/dashboard/monitor: people + groups with an Editor/Viewer role, and the
 * visibility tier. Public links/embeds are a separate feature. */
export function ShareDialog({
  objectType,
  objectId,
  ownerId,
  visibility,
  open,
  canManage,
  onClose,
  onChanged,
}: ShareDialogProps) {
  const qc = useQueryClient();
  const { has } = usePermissions();
  const aclKey = ["acl", objectType, objectId] as const;
  const [error, setError] = useState<string | null>(null);
  const [tier, setTier] = useState<Visibility>(visibility);
  const [granteeFilter, setGranteeFilter] = useState("");

  // Stays mounted between opens; reset transient state on reopen.
  useEffect(() => {
    if (open) {
      setTier(visibility);
      setError(null);
      setGranteeFilter("");
    }
  }, [open, visibility]);

  const { data: grants = [], isLoading } = useQuery({
    queryKey: aclKey,
    queryFn: () => api.listAcl(objectType, objectId),
    enabled: open,
  });
  const { data: users = [] } = useQuery({
    queryKey: ["users"],
    queryFn: () => api.listUsers(),
    enabled: open,
  });
  const { data: groups = [] } = useQuery({
    queryKey: ["groups"],
    queryFn: () => api.listGroups(),
    enabled: open && has("view_groups"),
  });

  const userName = (id: number) => users.find((u) => u.id === id)?.name ?? `User #${id}`;
  const groupName = (id: number) => groups.find((g) => g.id === id)?.name ?? `Group #${id}`;

  const invalidate = () => qc.invalidateQueries({ queryKey: aclKey });

  const grant = useMutation({
    mutationFn: (body: { grantee_id?: number; grantee_group_id?: number; access_type: ObjectRole }) =>
      api.grantAcl(objectType, objectId, body),
    onSuccess: invalidate,
    onError: (e) => setError(apiErrorMessage(e, "Failed to update access")),
  });

  const revoke = useMutation({
    mutationFn: (permId: number) => api.revokeAcl(objectType, objectId, permId),
    onSuccess: invalidate,
    onError: (e) => setError(apiErrorMessage(e, "Failed to remove access")),
  });

  const setVis = useMutation({
    mutationFn: (v: Visibility) => api.setVisibility(objectType, objectId, v),
    onSuccess: (_res, v) => {
      setTier(v);
      onChanged?.();
    },
    onError: (e) => setError(apiErrorMessage(e, "Failed to change visibility")),
  });

  const grantedUserIds = new Set([ownerId, ...grants.map((g) => g.grantee_id)]);
  const grantedGroupIds = new Set(grants.map((g) => g.grantee_group_id));
  const byName = (a: { name: string }, b: { name: string }) => a.name.localeCompare(b.name);
  const filteredUsers = users
    .filter(
      (u) =>
        !grantedUserIds.has(u.id) &&
        matchesSearch(granteeFilter, u.name, u.email),
    )
    .sort(byName);
  const filteredGroups = groups
    .filter((g) => !grantedGroupIds.has(g.id) && matchesSearch(granteeFilter, g.name))
    .sort(byName);

  function addGrantee(value: string) {
    setError(null);
    const [kind, idStr] = value.split(":");
    const id = Number(idStr);
    if (kind === "u") grant.mutate({ grantee_id: id, access_type: "viewer" });
    else grant.mutate({ grantee_group_id: id, access_type: "viewer" });
  }

  function changeRole(g: AccessPermission, role: ObjectRole) {
    setError(null);
    if (g.grantee_id != null) grant.mutate({ grantee_id: g.grantee_id, access_type: role });
    else if (g.grantee_group_id != null)
      grant.mutate({ grantee_group_id: g.grantee_group_id, access_type: role });
  }

  return (
    <Dialog open={open} onOpenChange={(o) => !o && onClose()}>
      <DialogContent className="max-h-[85dvh] max-w-lg overflow-y-auto">
        <DialogHeader>
          <DialogTitle>Share</DialogTitle>
        </DialogHeader>

        <div className="space-y-5 pt-2">
          <div>
            <p className="mb-2 text-sm font-medium">Visibility</p>
            <div className="flex gap-2">
              {(["workspace", "restricted"] as const).map((v) => (
                <Button
                  key={v}
                  type="button"
                  size="sm"
                  variant={tier === v ? "default" : "outline"}
                  aria-pressed={tier === v}
                  disabled={!canManage || setVis.isPending}
                  onClick={() => setVis.mutate(v)}
                >
                  {v === "workspace" ? <Users className="size-4" /> : <Lock className="size-4" />}
                  {v === "workspace" ? "Everyone" : "Specific people"}
                </Button>
              ))}
            </div>
            <p className="mt-1 text-xs text-muted-foreground">
              {VISIBILITY_HELP[tier === "restricted" ? "restricted" : "workspace"]}
            </p>
          </div>

          <div>
            <p className="mb-2 text-sm font-medium">People &amp; groups</p>

            {canManage && (
              <>
                <Input
                  placeholder="Search people or groups…"
                  value={granteeFilter}
                  onChange={(e) => setGranteeFilter(e.target.value)}
                  className="mb-2"
                  data-testid="share-grantee-filter"
                />
                <Select value="" onValueChange={addGrantee}>
                  <SelectTrigger className="mb-2" aria-label="Add person or group">
                    <SelectValue placeholder="Add a person or group…" />
                  </SelectTrigger>
                  <SelectContent>
                    {filteredUsers.length === 0 && filteredGroups.length === 0 ? (
                      <p className="px-2 py-1.5 text-sm text-muted-foreground">No people or groups match</p>
                    ) : (
                      <>
                        {filteredUsers.map((u) => (
                          <SelectItem key={`u:${u.id}`} value={`u:${u.id}`}>
                            {u.name}
                          </SelectItem>
                        ))}
                        {filteredGroups.map((g) => (
                          <SelectItem key={`g:${g.id}`} value={`g:${g.id}`}>
                            {g.name} (group)
                          </SelectItem>
                        ))}
                      </>
                    )}
                  </SelectContent>
                </Select>
              </>
            )}

            <ul className="space-y-1">
              {/* Owner — static, never removable here (transfer is separate). */}
              <li className="flex items-center gap-2 py-1">
                <OwnerChip name={userName(ownerId)} showName />
                <span className="ml-auto text-xs text-muted-foreground">Owner</span>
              </li>

              {isLoading && <li className="py-1 text-sm text-muted-foreground">Loading…</li>}

              {grants.map((g) => {
                const isGroup = g.grantee_group_id != null;
                const name = isGroup ? groupName(g.grantee_group_id!) : userName(g.grantee_id!);
                const role: ObjectRole = g.access_type;
                return (
                  <li key={g.id} className="flex items-center gap-2 py-1">
                    {isGroup ? (
                      <Users className="size-4 text-muted-foreground" />
                    ) : (
                      <User className="size-4 text-muted-foreground" />
                    )}
                    <span className="truncate text-sm">{name}</span>
                    <div className="ml-auto flex items-center gap-1">
                      {role === "owner" ? (
                        <span className="text-xs capitalize text-muted-foreground">owner</span>
                      ) : canManage ? (
                        <Select value={role} onValueChange={(r) => changeRole(g, r as ObjectRole)}>
                          <SelectTrigger size="sm" className="w-28" aria-label={`Role for ${name}`}>
                            <SelectValue />
                          </SelectTrigger>
                          <SelectContent>
                            <SelectItem value="editor">Editor</SelectItem>
                            <SelectItem value="viewer">Viewer</SelectItem>
                          </SelectContent>
                        </Select>
                      ) : (
                        <span className="text-xs capitalize text-muted-foreground">{role}</span>
                      )}
                      {canManage && (
                        <Button
                          type="button"
                          variant="ghost"
                          size="icon-sm"
                          aria-label={`Remove ${name}`}
                          onClick={() => {
                            setError(null);
                            revoke.mutate(g.id);
                          }}
                        >
                          <X className="size-4" />
                        </Button>
                      )}
                    </div>
                  </li>
                );
              })}

              {!isLoading && grants.length === 0 && (
                <li className="py-1 text-sm text-muted-foreground">
                  {tier === "restricted"
                    ? "Not shared with anyone yet."
                    : "Visible to your whole workspace."}
                </li>
              )}
            </ul>
          </div>

          {error && <p className="text-sm text-destructive">{error}</p>}
        </div>

        <DialogFooter>
          <Button variant="outline" onClick={onClose}>
            {(grant.isPending || revoke.isPending || setVis.isPending) && <Loader size={16} />}
            Done
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
