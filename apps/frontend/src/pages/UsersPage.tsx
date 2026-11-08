import { useState } from "react";
import { useQuery, useQueries, useMutation, useQueryClient } from "@tanstack/react-query";
import { AlertTriangle, Search, X } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Dialog, DialogContent, DialogFooter, DialogHeader, DialogTitle, DialogTrigger } from "@/components/ui/dialog";
import { ConfirmDialog } from "@/components/ui/confirm-dialog";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { Badge } from "@/components/ui/badge";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import { PageLoader } from "@/components/ui/loader";
import { EmptyState } from "@/components/common/EmptyState";
import { ReadonlyField } from "@/components/common/ReadonlyField";
import { api, apiErrorMessage } from "@/lib/api";
import { copyToClipboard, matchesSearch } from "@/lib/utils";
import { useAuth, SESSION_QUERY_KEY } from "@/lib/auth";
import { usePermissions } from "@/lib/permissions";
import { useTransientMessage } from "@/hooks/useTransientMessage";
import { EMAIL_RE } from "@/lib/formValidation";
import type { Group, GroupRole, Permission, UserDetails } from "@/types/api";

// The group picker is a Portal-rendered Popover, DOM-outside the Dialog, so interaction in it must
// not read as "outside".
function keepPopoverInteractionInside(e: { target: EventTarget | null; preventDefault(): void }) {
  if (e.target instanceof Element && e.target.closest('[data-slot="popover-content"]')) {
    e.preventDefault();
  }
}

/** Builtins first, least privilege first; everything else alphabetical after. */
const BUILTIN_GROUP_ORDER = ["viewers", "default", "admin"];

function groupSortKey(g: Group): [number, number, string] {
  if (g.type !== "builtin") return [1, 0, g.name];
  const i = BUILTIN_GROUP_ORDER.indexOf(g.name);
  return [0, i === -1 ? BUILTIN_GROUP_ORDER.length : i, g.name];
}

/** null = roles not known yet (loading or failed) — render the name alone. */
function rolesLabel(roles: GroupRole[] | undefined): string | null {
  if (!roles) return null;
  if (roles.length === 0) return "No role assigned";
  return roles.map((r) => r.name).join(", ");
}

/** Checkbox popover picker for group membership; each row/chip shows the group's role(s) so picking
 * a group also picks a permission level. */
function GroupsField({
  groups,
  selectedIds,
  onChange,
  emptyHint = "No groups selected.",
  emptyHintId,
}: {
  groups: Group[];
  selectedIds: number[];
  onChange: (ids: number[]) => void;
  emptyHint?: string;
  emptyHintId?: string;
}) {
  const [open, setOpen] = useState(false);
  const [filter, setFilter] = useState("");
  const candidates = groups.filter((g) => matchesSearch(filter, g.name)).sort((a, b) => {
    const ka = groupSortKey(a);
    const kb = groupSortKey(b);
    return ka[0] - kb[0] || ka[1] - kb[1] || ka[2].localeCompare(kb[2]);
  });
  const selectedSet = new Set(selectedIds);

  // One roles request per group; fold into GET /api/groups if group counts make this costly.
  const roleQueries = useQueries({
    queries: groups.map((g) => ({ queryKey: ["group-roles", g.id], queryFn: () => api.listGroupRoles(g.id) })),
  });
  const rolesByGroupId = new Map(groups.map((g, i) => [g.id, roleQueries[i]?.data]));

  function toggle(id: number, checked: boolean) {
    onChange(checked ? [...selectedIds, id] : selectedIds.filter((x) => x !== id));
  }

  return (
    <div>
      <label className="text-sm font-medium">Groups</label>
      <div className="mt-1 flex flex-wrap gap-1">
        {selectedIds.map((id) => {
          const g = groups.find((x) => x.id === id);
          if (!g) return null;
          const label = rolesLabel(rolesByGroupId.get(g.id));
          return (
            <Badge key={id} variant="outline" className="max-w-full gap-1">
              <span className="truncate">
                {g.name}
                {label && <span className="text-muted-foreground"> — {label}</span>}
              </span>
              <button
                type="button"
                className="cursor-pointer"
                aria-label={`Remove ${g.name}`}
                title="Remove group"
                onClick={() => toggle(id, false)}
              >
                <X size={12} aria-hidden="true" />
              </button>
            </Badge>
          );
        })}
        {selectedIds.length === 0 && (
          <span id={emptyHintId} className="text-sm text-muted-foreground">
            {emptyHint}
          </span>
        )}
      </div>
      <Popover open={open} onOpenChange={setOpen}>
        <PopoverTrigger asChild>
          <Button type="button" variant="outline" size="sm" className="mt-2">
            Add group…
          </Button>
        </PopoverTrigger>
        <PopoverContent align="start" className="w-80 p-2">
          <Input
            autoFocus
            placeholder="Search…"
            value={filter}
            onChange={(e) => setFilter(e.target.value)}
            className="mb-2"
          />
          <div className="max-h-64 space-y-1 overflow-y-auto">
            {candidates.length === 0 && <p className="px-1 py-2 text-sm text-muted-foreground">No groups found.</p>}
            {candidates.map((g) => {
              const roles = rolesByGroupId.get(g.id);
              const label = rolesLabel(roles);
              const title =
                roles && roles.length === 0
                  ? `${g.name} — No role assigned — members of this group get no permissions.`
                  : label
                    ? `${g.name} — ${label}`
                    : g.name;
              return (
                <label
                  key={g.id}
                  title={title}
                  className="flex cursor-pointer items-center gap-2 rounded px-1 py-1 text-sm hover:bg-muted"
                >
                  <input
                    type="checkbox"
                    className="size-4 shrink-0 cursor-pointer accent-primary"
                    checked={selectedSet.has(g.id)}
                    onChange={(e) => toggle(g.id, e.target.checked)}
                  />
                  <span className="min-w-0 flex-1 truncate">
                    {g.name}
                    {label && <span className="text-xs text-muted-foreground"> — {label}</span>}
                  </span>
                </label>
              );
            })}
          </div>
        </PopoverContent>
      </Popover>
    </div>
  );
}

/** Read-only derived view of a user's reach. `groupIds` is the dialog's unsaved selection, falling
 * back to the persisted snapshot while roles load or fail, rather than flashing empty. */
function EffectivePermissionsSection({ userId, groupIds }: { userId: number; groupIds: number[] }) {
  const { data: catalog = [] } = useQuery({ queryKey: ["permissions"], queryFn: () => api.listPermissions() });
  const { data: persisted } = useQuery({
    queryKey: ["effective-permissions", userId],
    queryFn: () => api.getUserEffectivePermissions(userId),
  });

  // Same union-of-role-permissions algorithm as authz::permissions::load_effective_groups.
  const groupRoleQueries = useQueries({
    queries: groupIds.map((id) => ({ queryKey: ["group-roles", id], queryFn: () => api.listGroupRoles(id) })),
  });
  const liveReady = groupRoleQueries.every((q) => q.isSuccess);
  const livePermissions = liveReady
    ? [...new Set(groupRoleQueries.flatMap((q) => (q.data ?? []).flatMap((role) => role.permissions)))]
    : null;

  const effective = livePermissions
    ? { permissions: livePermissions, is_admin: livePermissions.includes("admin") }
    : persisted;

  if (!effective) return null;

  const infoByKey = new Map(catalog.map((p) => [p.key, p]));
  const grouped = new Map<string, Permission[]>();
  for (const key of effective.permissions) {
    const info = infoByKey.get(key);
    if (info) grouped.set(info.category, [...(grouped.get(info.category) ?? []), info]);
  }

  return (
    <div className="space-y-2 border-t pt-4">
      <div>
        <h3 className="text-sm font-medium">Effective permissions</h3>
        <p className="text-xs text-muted-foreground">
          Derived from this user&apos;s groups and roles. To change them, edit the user&apos;s
          groups or the roles those groups use.
        </p>
      </div>
      {effective.is_admin && <Badge variant="destructive">Workspace admin</Badge>}
      {effective.permissions.length === 0 ? (
        <p className="text-sm text-muted-foreground">No permissions yet. Add this user to a group to grant access.</p>
      ) : (
        [...grouped].map(([cat, perms]) => (
          <div key={cat}>
            <p className="mb-1 text-xs font-medium uppercase text-muted-foreground">{cat}</p>
            <div className="flex flex-wrap gap-1">
              {perms.map((p) => (
                <Badge key={p.key} variant="secondary">
                  {p.label}
                </Badge>
              ))}
            </div>
          </div>
        ))
      )}
    </div>
  );
}

/** A group's data-source scoping posture from `GET /api/groups/:id/data_sources`; kept minimal so
 * it can't drift into re-deriving authz's rule. */
export interface GroupScopePosture {
  id: number;
  name: string;
  restricted: boolean;
}

/** Restricted groups whose allowlist is inert under `authz::data_source::in_scope`'s union rule
 * (`crates/shared/src/authz/data_source.rs`): any unrestricted group in the set already grants
 * every data source. */
export function overriddenRestrictedGroups(selected: GroupScopePosture[]): GroupScopePosture[] {
  const hasUnrestricted = selected.some((g) => !g.restricted);
  return hasUnrestricted ? selected.filter((g) => g.restricted) : [];
}

/** Inline warning, not a gate. Fetches postures only once 2+ groups are selected, since fewer can't
 * mix. */
function ScopeOverrideWarning({
  userName,
  groupIds,
  groups,
}: {
  userName: string;
  groupIds: number[];
  groups: Group[];
}) {
  const selectedGroups = groupIds
    .map((id) => groups.find((g) => g.id === id))
    .filter((g): g is Group => g !== undefined);
  const needsCheck = selectedGroups.length >= 2;

  const restrictionQueries = useQueries({
    queries: selectedGroups.map((g) => ({
      queryKey: ["group-data-sources", g.id],
      queryFn: () => api.listGroupDataSources(g.id),
      enabled: needsCheck,
    })),
  });

  if (!needsCheck || !restrictionQueries.every((q) => q.isSuccess)) return null;

  const postures: GroupScopePosture[] = selectedGroups.map((g, i) => ({
    id: g.id,
    name: g.name,
    restricted: restrictionQueries[i]!.data!.restricted,
  }));
  const overridden = overriddenRestrictedGroups(postures);
  if (overridden.length === 0) return null;

  const names = overridden.map((g) => g.name);
  const isPlural = names.length > 1;
  const namesLabel =
    names.length <= 2 ? names.join(" and ") : `${names.slice(0, -1).join(", ")}, and ${names[names.length - 1]}`;

  return (
    <div
      aria-live="polite"
      className="flex items-start gap-2 rounded-md border border-warning/40 bg-warning/10 p-3"
    >
      <AlertTriangle className="size-4 shrink-0 mt-0.5 text-warning" aria-hidden="true" />
      <p className="text-sm text-warning">
        Because {userName} is also in an unrestricted group, the data source restriction
        {isPlural ? "s" : ""} on {namesLabel} no longer appl{isPlural ? "y" : "ies"} — {userName} can
        see every data source in the organization.
      </p>
    </div>
  );
}

function EditUserDialog({ user, onClose }: { user: UserDetails; onClose: () => void }) {
  const queryClient = useQueryClient();
  const [name, setName] = useState(user.name);
  const [groupIds, setGroupIds] = useState<number[]>(user.groups);
  const [error, setError] = useState<string | null>(null);
  const { data: groups = [] } = useQuery({ queryKey: ["groups"], queryFn: () => api.listGroups() });

  const editMutation = useMutation({
    mutationFn: () => api.updateUser(user.id, { name, groups: groupIds }),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["users"] });
      queryClient.invalidateQueries({ queryKey: SESSION_QUERY_KEY });
      queryClient.invalidateQueries({ queryKey: ["effective-permissions", user.id] });
      onClose();
    },
    onError: (err) => setError(apiErrorMessage(err, "Failed to save")),
  });

  return (
    <Dialog open onOpenChange={onClose}>
      <DialogContent onInteractOutside={keepPopoverInteractionInside}>
        <DialogHeader>
          <DialogTitle>Edit User</DialogTitle>
        </DialogHeader>
        <div className="space-y-4 pt-2">
          <div>
            <label htmlFor="edit-user-name" className="text-sm font-medium">Name</label>
            <Input id="edit-user-name" value={name} onChange={(e) => setName(e.target.value)} />
          </div>
          <ReadonlyField id="edit-user-email" label="Email" value={user.email} />
          <GroupsField groups={groups} selectedIds={groupIds} onChange={setGroupIds} />
          <ScopeOverrideWarning userName={user.name} groupIds={groupIds} groups={groups} />
          {error && <p className="text-sm text-destructive">{error}</p>}
          <EffectivePermissionsSection userId={user.id} groupIds={groupIds} />
        </div>
        <DialogFooter>
          <Button variant="outline" onClick={onClose}>
            Cancel
          </Button>
          <Button
            onClick={() => { setError(null); editMutation.mutate(); }}
            disabled={editMutation.isPending || !name}
          >
            {editMutation.isPending ? "Saving…" : "Save"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

export function UsersPage() {
  const queryClient = useQueryClient();
  const { user: sessionUser } = useAuth();
  const { has } = usePermissions();
  const canManage = has("manage_users");
  const [open, setOpen] = useState(false);
  const [name, setName] = useState("");
  const [email, setEmail] = useState("");
  const [groupIds, setGroupIds] = useState<number[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [search, setSearch] = useState("");
  const [pendingDisable, setPendingDisable] = useState<UserDetails | null>(null);
  const [disableError, setDisableError] = useState<string | null>(null);
  const [enableError, setEnableError] = useState<string | null>(null);
  const [pendingRevoke, setPendingRevoke] = useState<UserDetails | null>(null);
  const [revokeError, setRevokeError] = useState<string | null>(null);
  const [resendFeedback, setResendFeedback] = useTransientMessage<{ id: number; kind: "ok" | "error"; text: string }>();
  const [editingUser, setEditingUser] = useState<UserDetails | null>(null);
  const [regenUser, setRegenUser] = useState<UserDetails | null>(null);
  const [regenError, setRegenError] = useState<string | null>(null);
  const [copied, setCopied] = useTransientMessage<true>(2000);
  const [copyFailed, setCopyFailed] = useState(false);

  // include_disabled=true so disabled users still show the "Disabled" badge instead of vanishing.
  const usersQuery = useQuery({ queryKey: ["users", { includeDisabled: true }], queryFn: () => api.listUsers(true) });
  const groupsQuery = useQuery({ queryKey: ["groups"], queryFn: () => api.listGroups() });

  function closeInviteDialog() {
    setOpen(false);
    setName("");
    setEmail("");
    setGroupIds([]);
    setError(null);
  }

  const inviteMutation = useMutation({
    mutationFn: () => api.inviteUser({ name, email, groups: groupIds }),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["users"] });
      queryClient.invalidateQueries({ queryKey: SESSION_QUERY_KEY });
      closeInviteDialog();
    },
    onError: (err) => setError(apiErrorMessage(err, "Failed to invite")),
  });

  const disableMutation = useMutation({
    mutationFn: (id: number) => api.deleteUser(id),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["users"] });
      queryClient.invalidateQueries({ queryKey: SESSION_QUERY_KEY });
      setPendingDisable(null);
    },
    onError: (err) => setDisableError(apiErrorMessage(err, "Failed to disable user")),
  });

  const enableMutation = useMutation({
    mutationFn: (id: number) => api.updateUser(id, { disabled: false }),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["users"] });
      queryClient.invalidateQueries({ queryKey: SESSION_QUERY_KEY });
      setEnableError(null);
    },
    onError: (err) => setEnableError(apiErrorMessage(err, "Failed to enable user")),
  });

  const regenMutation = useMutation({
    mutationFn: (id: number) => api.regenerateApiKey(id),
    onSuccess: () => {
      setRegenError(null);
      queryClient.invalidateQueries({ queryKey: ["users"] });
    },
    onError: (err) => setRegenError(apiErrorMessage(err, "Failed to regenerate API key")),
  });

  const resendMutation = useMutation({
    mutationFn: (id: number) => api.resendInvitation(id),
    onSuccess: (_d, id) => {
      queryClient.invalidateQueries({ queryKey: ["users"] });
      setResendFeedback({ id, kind: "ok", text: "Invitation resent." });
    },
    onError: (err, id) =>
      setResendFeedback({ id, kind: "error", text: apiErrorMessage(err, "Failed to resend invitation") }),
  });

  const revokeMutation = useMutation({
    mutationFn: (id: number) => api.revokeInvitation(id),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["users"] });
      setPendingRevoke(null);
    },
    onError: (err) => setRevokeError(apiErrorMessage(err, "Failed to revoke invitation")),
  });

  function closeDisableDialog() {
    setPendingDisable(null);
    setDisableError(null);
  }

  function closeRevokeDialog() {
    setPendingRevoke(null);
    setRevokeError(null);
  }

  function closeRegenDialog() {
    setRegenUser(null);
    regenMutation.reset();
    setRegenError(null);
    setCopied(null);
    setCopyFailed(false);
  }

  async function copyNewKey() {
    if (!regenMutation.data) return;
    const ok = await copyToClipboard(regenMutation.data.api_key);
    setCopyFailed(!ok);
    if (ok) setCopied(true);
  }

  const groupNameById = new Map((groupsQuery.data ?? []).map((g) => [g.id, g.name]));
  const emailValid = EMAIL_RE.test(email.trim());

  const q = search.trim();
  const filtered = usersQuery.data?.filter((u) => matchesSearch(q, u.name, u.email));

  return (
    <div className="space-y-6">
      <div className="flex items-center justify-between">
        <Input
          aria-label="Search users"
          placeholder="Search users…"
          className="max-w-md"
          value={search}
          onChange={(e) => setSearch(e.target.value)}
        />
        {canManage && (
          <Dialog open={open} onOpenChange={(o) => (o ? setOpen(true) : closeInviteDialog())}>
            <DialogTrigger asChild>
              <Button>+ Invite User</Button>
            </DialogTrigger>
            <DialogContent onInteractOutside={keepPopoverInteractionInside}>
              <DialogHeader>
                <DialogTitle>Invite User</DialogTitle>
              </DialogHeader>
              <div className="space-y-4 pt-2">
                <div>
                  <label htmlFor="invite-user-name" className="text-sm font-medium">Name</label>
                  <Input id="invite-user-name" value={name} onChange={(e) => setName(e.target.value)} placeholder="Jane Smith" />
                </div>
                <div>
                  <label htmlFor="invite-user-email" className="text-sm font-medium">Email</label>
                  <Input
                    id="invite-user-email"
                    type="email"
                    value={email}
                    onChange={(e) => setEmail(e.target.value)}
                    placeholder="jane@example.com"
                    aria-invalid={email !== "" && !emailValid}
                  />
                  {email !== "" && !emailValid && (
                    <p className="text-sm text-destructive">Enter a valid email address.</p>
                  )}
                </div>
                <GroupsField
                  groups={groupsQuery.data ?? []}
                  selectedIds={groupIds}
                  onChange={setGroupIds}
                  emptyHint="Pick at least one group — groups decide what this user can see and do."
                  emptyHintId="invite-groups-hint"
                />
                <ScopeOverrideWarning
                  userName={name || "This user"}
                  groupIds={groupIds}
                  groups={groupsQuery.data ?? []}
                />
                {error && (
                  <p aria-live="polite" className="text-sm text-destructive">{error}</p>
                )}
                <Button
                  onClick={() => { setError(null); inviteMutation.mutate(); }}
                  disabled={inviteMutation.isPending || !name || !emailValid || groupIds.length === 0}
                  title={groupIds.length === 0 ? "Pick at least one group first" : undefined}
                  aria-describedby={groupIds.length === 0 ? "invite-groups-hint" : undefined}
                >
                  {inviteMutation.isPending ? "Inviting…" : "Send Invitation"}
                </Button>
              </div>
            </DialogContent>
          </Dialog>
        )}
      </div>

      {usersQuery.isLoading && <PageLoader />}
      {usersQuery.isError && (
        <p className="text-destructive">
          {apiErrorMessage(usersQuery.error, "Failed to load")}
        </p>
      )}
      {filtered && filtered.length === 0 && q && (
        <EmptyState icon={Search} title="No users match your search." />
      )}
      {enableError && <p className="text-sm text-destructive">{enableError}</p>}
      {filtered && (filtered.length > 0 || !q) && (
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead>Name</TableHead>
              <TableHead className="hidden md:table-cell">Email</TableHead>
              <TableHead>Groups</TableHead>
              <TableHead>Status</TableHead>
              {canManage && <TableHead>Actions</TableHead>}
            </TableRow>
          </TableHeader>
          <TableBody>
            {filtered.map((u) => {
              const isSelf = u.id === sessionUser?.id;
              const enablePending = enableMutation.isPending && enableMutation.variables === u.id;
              const resendPending = resendMutation.isPending && resendMutation.variables === u.id;
              return (
                <TableRow key={u.id}>
                  <TableCell className={u.disabled ? "text-muted-foreground" : undefined}>
                    {u.name}
                    <div className="text-xs text-muted-foreground md:hidden truncate">{u.email}</div>
                  </TableCell>
                  <TableCell className={`hidden md:table-cell${u.disabled ? " text-muted-foreground" : ""}`}>
                    {u.email}
                  </TableCell>
                  <TableCell>
                    <div className="flex flex-wrap gap-1">
                      {u.groups.map((id) => {
                        const name = groupNameById.get(id);
                        if (name) return <Badge key={id} variant="outline">{name}</Badge>;
                        // A failed groups fetch (e.g. 403 without view_groups) means an incomplete
                        // map, not bogus ids — don't claim "Unknown".
                        return (
                          <Badge
                            key={id}
                            variant="outline"
                            className="text-muted-foreground"
                            title={
                              groupsQuery.isError
                                ? "Group names aren't visible without the view_groups permission."
                                : undefined
                            }
                          >
                            —
                          </Badge>
                        );
                      })}
                      {u.groups.length === 0 && <span className="text-muted-foreground">—</span>}
                    </div>
                  </TableCell>
                  <TableCell>
                    {u.is_invitation_pending ? (
                      <Badge variant="secondary">Invited</Badge>
                    ) : u.disabled ? (
                      <Badge variant="destructive">Disabled</Badge>
                    ) : (
                      <Badge variant="outline">Active</Badge>
                    )}
                  </TableCell>
                  {canManage && (
                    <TableCell>
                      <div className="flex flex-wrap items-center gap-1">
                        {u.is_invitation_pending ? (
                          <>
                            <Button variant="ghost" size="sm" onClick={() => setEditingUser(u)}>
                              Edit
                            </Button>
                            <Button
                              variant="ghost"
                              size="sm"
                              onClick={() => { setResendFeedback(null); resendMutation.mutate(u.id); }}
                              disabled={resendPending}
                            >
                              {resendPending ? "Resending…" : "Resend invitation"}
                            </Button>
                            <Button
                              variant="ghost"
                              size="sm"
                              className="text-destructive"
                              onClick={() => { setRevokeError(null); setPendingRevoke(u); }}
                            >
                              Revoke
                            </Button>
                            {resendFeedback?.id === u.id && (
                              <p
                                aria-live="polite"
                                className={`self-center text-xs ${resendFeedback.kind === "error" ? "text-destructive" : "text-muted-foreground"}`}
                              >
                                {resendFeedback.text}
                              </p>
                            )}
                          </>
                        ) : u.disabled ? (
                          <Button
                            variant="ghost"
                            size="sm"
                            onClick={() => { setEnableError(null); enableMutation.mutate(u.id); }}
                            disabled={enablePending}
                          >
                            {enablePending ? "Enabling…" : "Enable"}
                          </Button>
                        ) : (
                          <>
                            <Button variant="ghost" size="sm" onClick={() => setEditingUser(u)}>
                              Edit
                            </Button>
                            <Button variant="ghost" size="sm" onClick={() => setRegenUser(u)}>
                              Regenerate key
                            </Button>
                            {!isSelf && (
                              <Button
                                variant="ghost"
                                size="sm"
                                className="text-destructive"
                                onClick={() => { setDisableError(null); setPendingDisable(u); }}
                              >
                                Disable
                              </Button>
                            )}
                          </>
                        )}
                      </div>
                    </TableCell>
                  )}
                </TableRow>
              );
            })}
          </TableBody>
        </Table>
      )}

      {editingUser && (
        <EditUserDialog key={editingUser.id} user={editingUser} onClose={() => setEditingUser(null)} />
      )}

      <ConfirmDialog
        open={pendingDisable !== null}
        onOpenChange={(o) => !o && closeDisableDialog()}
        title="Disable user?"
        body={
          <p className="text-sm text-muted-foreground">
            {pendingDisable?.name} ({pendingDisable?.email}) will be signed out and unable to log
            in. Their queries, dashboards, and monitors remain. You can enable them again later.
          </p>
        }
        error={disableError}
        destructive
        confirmLabel="Disable"
        pendingLabel="Disabling…"
        confirmPending={disableMutation.isPending}
        allowCancelWhilePending
        onConfirm={() => { setDisableError(null); if (pendingDisable) disableMutation.mutate(pendingDisable.id); }}
      />

      <ConfirmDialog
        open={pendingRevoke !== null}
        onOpenChange={(o) => !o && closeRevokeDialog()}
        title="Revoke invitation?"
        body={
          <p className="text-sm text-muted-foreground">
            The invitation for {pendingRevoke?.name} ({pendingRevoke?.email}) will be cancelled
            and their invite link stops working. This can&apos;t be undone — invite them again
            from scratch if you change your mind.
          </p>
        }
        error={revokeError}
        destructive
        confirmLabel="Revoke"
        pendingLabel="Revoking…"
        confirmPending={revokeMutation.isPending}
        allowCancelWhilePending
        onConfirm={() => { setRevokeError(null); if (pendingRevoke) revokeMutation.mutate(pendingRevoke.id); }}
      />

      <Dialog open={regenUser !== null} onOpenChange={(o) => !o && closeRegenDialog()}>
        <DialogContent>
          {regenMutation.isSuccess ? (
            <>
              <DialogHeader>
                <DialogTitle>New API key</DialogTitle>
              </DialogHeader>
              <div className="space-y-2">
                <div className="flex gap-2">
                  <Input readOnly value={regenMutation.data.api_key} className="font-mono text-xs" />
                  <Button size="sm" variant="outline" onClick={copyNewKey}>
                    {copied ? "Copied" : "Copy"}
                  </Button>
                </div>
                {copyFailed && (
                  <p className="text-sm text-destructive">
                    Couldn&rsquo;t copy automatically — select the key above and copy it before
                    closing. It won&rsquo;t be shown again.
                  </p>
                )}
                <p className="text-xs text-muted-foreground">
                  Store it now. It will not be shown again after you close this dialog.
                </p>
              </div>
              <DialogFooter>
                <Button onClick={closeRegenDialog}>Done</Button>
              </DialogFooter>
            </>
          ) : (
            <>
              <DialogHeader>
                <DialogTitle>Regenerate API key?</DialogTitle>
              </DialogHeader>
              <p className="text-sm text-muted-foreground">
                The current API key for {regenUser?.name} stops working immediately. Anything
                using it — scheduled scripts, embeds, integrations — will fail until it is
                updated with the new key.
              </p>
              {regenError && <p className="text-sm text-destructive">{regenError}</p>}
              <DialogFooter>
                <Button variant="outline" onClick={closeRegenDialog}>
                  Cancel
                </Button>
                <Button
                  variant="destructive"
                  disabled={regenMutation.isPending}
                  onClick={() => { setRegenError(null); if (regenUser) regenMutation.mutate(regenUser.id); }}
                >
                  {regenMutation.isPending ? "Regenerating…" : "Regenerate"}
                </Button>
              </DialogFooter>
            </>
          )}
        </DialogContent>
      </Dialog>
    </div>
  );
}
