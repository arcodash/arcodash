import { useState } from "react";
import { useQuery, useMutation, useQueryClient } from "@tanstack/react-query";
import { X, UsersRound, AlertTriangle, Lock, Search } from "lucide-react";
import { api, apiErrorMessage } from "@/lib/api";
import type { Group } from "@/types/api";
import { Loader, PageLoader } from "@/components/ui/loader";
import { Input } from "@/components/ui/input";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import { Switch } from "@/components/ui/switch";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { Dialog, DialogContent, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog";
import { ConfirmDialog } from "@/components/ui/confirm-dialog";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import { EmptyState } from "@/components/common/EmptyState";
import { DocsLink } from "@/components/common/DocsLink";
import { MultiSelectRoles } from "@/components/access/MultiSelectRoles";
import { usePermissions } from "@/lib/permissions";
import { SESSION_QUERY_KEY } from "@/lib/auth";
import { matchesSearch } from "@/lib/utils";

function PickerPopover<T extends { id: number }>({
  triggerLabel,
  items,
  renderLabel,
  onSelect,
}: {
  triggerLabel: string;
  items: T[];
  renderLabel: (item: T) => string;
  onSelect: (item: T) => void;
}) {
  const [open, setOpen] = useState(false);
  const [filter, setFilter] = useState("");
  const candidates = items.filter((item) => matchesSearch(filter, renderLabel(item)));

  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger asChild>
        <Button type="button" variant="outline" size="sm">
          {triggerLabel}
        </Button>
      </PopoverTrigger>
      <PopoverContent align="start" className="w-72 p-2">
        <Input
          autoFocus
          placeholder="Search…"
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
          className="mb-2"
        />
        <div className="max-h-64 space-y-1 overflow-y-auto">
          {candidates.length === 0 && <p className="px-1 py-2 text-sm text-muted-foreground">No matches.</p>}
          {candidates.map((item) => (
            <button
              key={item.id}
              type="button"
              className="block w-full cursor-pointer rounded px-2 py-1 text-left text-sm hover:bg-muted"
              onClick={() => {
                onSelect(item);
                setFilter("");
              }}
            >
              {renderLabel(item)}
            </button>
          ))}
        </div>
      </PopoverContent>
    </Popover>
  );
}

function GroupRolesCell({ groupId }: { groupId: number }) {
  const { data: roles = [], isPending } = useQuery({
    queryKey: ["group-roles", groupId],
    queryFn: () => api.listGroupRoles(groupId),
  });
  if (isPending) return <span className="text-muted-foreground">—</span>;
  if (roles.length === 0) {
    return (
      <span className="flex items-center gap-1.5 text-muted-foreground">
        —
        <span
          aria-label="No role assigned — members of this group get no permissions."
          title="No role assigned — members of this group get no permissions."
        >
          <AlertTriangle size={14} className="text-warning" aria-hidden="true" />
        </span>
      </span>
    );
  }
  const shown = roles.slice(0, 2);
  const rest = roles.length - shown.length;
  return (
    <div className="flex flex-wrap gap-1">
      {shown.map((r) => (
        <Badge key={r.id} variant="outline">
          {r.name}
        </Badge>
      ))}
      {rest > 0 && <Badge variant="outline">+{rest}</Badge>}
    </div>
  );
}

function GroupMemberCountCell({ groupId }: { groupId: number }) {
  const { data: members = [], isPending } = useQuery({
    queryKey: ["group-members", groupId],
    queryFn: () => api.listGroupMembers(groupId),
  });
  return <span className="text-muted-foreground">{isPending ? "—" : members.length}</span>;
}

function GroupRestrictedBadge({ groupId }: { groupId: number }) {
  const { data, isPending, isError } = useQuery({
    queryKey: ["group-data-sources", groupId],
    queryFn: () => api.listGroupDataSources(groupId),
  });
  // An unknown value must never render as the permissive "unrestricted"; show it explicitly.
  const unknown = isPending || isError;
  if (!unknown && !data.restricted) return null;
  return (
    <Badge
      variant="outline"
      className="gap-1 text-muted-foreground"
      title={
        unknown
          ? "Checking data-source access…"
          : "This group can only use the data sources it's been given access to."
      }
    >
      <Lock className="size-3" aria-hidden="true" />
      {unknown ? (
        <>
          <span className="sr-only">Access level unknown</span>—
        </>
      ) : (
        "Restricted"
      )}
    </Badge>
  );
}

export default function GroupsPage() {
  const qc = useQueryClient();
  const { has } = usePermissions();
  const canManage = has("manage_groups");
  const { data: groups = [], isLoading, isError } = useQuery({
    queryKey: ["groups"],
    queryFn: () => api.listGroups(),
  });

  const [search, setSearch] = useState("");
  const filteredGroups = groups.filter((g) => matchesSearch(search, g.name));

  const [createOpen, setCreateOpen] = useState(false);
  const [newName, setNewName] = useState("");
  const [createError, setCreateError] = useState<string | null>(null);
  const createGroup = useMutation({
    mutationFn: (name: string) => api.createGroup({ name }),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["groups"] });
      qc.invalidateQueries({ queryKey: SESSION_QUERY_KEY });
      setNewName("");
      setCreateError(null);
      setCreateOpen(false);
    },
    onError: (err) => setCreateError(apiErrorMessage(err, "Failed to create group")),
  });

  const openCreate = () => {
    setCreateError(null);
    setCreateOpen(true);
  };

  const [manageGroup, setManageGroup] = useState<Group | null>(null);

  const [deleteTarget, setDeleteTarget] = useState<Group | null>(null);
  const [deleteError, setDeleteError] = useState<string | null>(null);
  const deleteGroup = useMutation({
    mutationFn: (id: number) => api.deleteGroup(id),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["groups"] });
      qc.invalidateQueries({ queryKey: SESSION_QUERY_KEY });
      setDeleteTarget(null);
    },
    onError: (err) => setDeleteError(apiErrorMessage(err, "Failed to delete group")),
  });

  const closeDelete = () => {
    setDeleteTarget(null);
    setDeleteError(null);
  };

  if (isError) {
    return <p className="text-destructive">Failed to load groups.</p>;
  }

  return (
    <div className="space-y-4">
      <p className="text-sm text-muted-foreground">
        Groups control which members can see and query which data sources.{" "}
        <DocsLink topic="groupAccess" label="group data source access" />
      </p>
      <div className="flex items-center justify-between gap-2">
        <Input
          aria-label="Search groups"
          placeholder="Search groups…"
          className="max-w-sm"
          value={search}
          onChange={(e) => setSearch(e.target.value)}
        />
        {canManage && <Button onClick={openCreate}>+ New group</Button>}
      </div>

      {isLoading && <PageLoader />}

      {!isLoading && groups.length === 0 && (
        <EmptyState
          icon={UsersRound}
          title="No groups yet."
          description="Create a group to organize members and grant access."
          action={canManage ? <Button onClick={openCreate}>+ New group</Button> : undefined}
        />
      )}

      {!isLoading && groups.length > 0 && filteredGroups.length === 0 && (
        <EmptyState icon={Search} title="No groups match your search." />
      )}

      {filteredGroups.length > 0 && (
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead>Name</TableHead>
              <TableHead>Type</TableHead>
              <TableHead>Roles</TableHead>
              <TableHead>Members</TableHead>
              {canManage && <TableHead>Actions</TableHead>}
            </TableRow>
          </TableHeader>
          <TableBody>
            {filteredGroups.map((g) => (
              <TableRow key={g.id}>
                <TableCell>
                  <div className="flex items-center gap-2">
                    <span>{g.name}</span>
                    <GroupRestrictedBadge groupId={g.id} />
                  </div>
                </TableCell>
                <TableCell className="text-muted-foreground">{g.type}</TableCell>
                <TableCell>
                  <GroupRolesCell groupId={g.id} />
                </TableCell>
                <TableCell>
                  <GroupMemberCountCell groupId={g.id} />
                </TableCell>
                {canManage && (
                  <TableCell>
                    <div className="flex gap-1">
                      <Button variant="ghost" size="sm" onClick={() => setManageGroup(g)}>
                        Manage
                      </Button>
                      {g.type !== "builtin" && (
                        <Button
                          variant="ghost"
                          size="sm"
                          className="text-destructive"
                          onClick={() => {
                            setDeleteError(null);
                            setDeleteTarget(g);
                          }}
                        >
                          Delete
                        </Button>
                      )}
                    </div>
                  </TableCell>
                )}
              </TableRow>
            ))}
          </TableBody>
        </Table>
      )}

      <Dialog open={createOpen} onOpenChange={setCreateOpen}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>New group</DialogTitle>
          </DialogHeader>
          <Input
            autoFocus
            value={newName}
            onChange={(e) => setNewName(e.target.value)}
            placeholder="Group name"
          />
          {createError && <p className="text-sm text-destructive">{createError}</p>}
          <DialogFooter>
            <Button variant="outline" onClick={() => setCreateOpen(false)}>
              Cancel
            </Button>
            <Button
              disabled={!newName.trim() || createGroup.isPending}
              onClick={() => createGroup.mutate(newName.trim())}
            >
              {createGroup.isPending ? (
                <>
                  <Loader size={16} />
                  Creating…
                </>
              ) : (
                "Create"
              )}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      {manageGroup && <ManageGroupDialog group={manageGroup} onClose={() => setManageGroup(null)} />}

      <ConfirmDialog
        open={deleteTarget !== null}
        onOpenChange={(o) => !o && closeDelete()}
        title={`Delete group "${deleteTarget?.name ?? ""}"?`}
        body={
          <p className="text-sm text-muted-foreground">
            This group will be removed. Its members lose any access granted only through this group.
          </p>
        }
        error={deleteError}
        destructive
        confirmLabel="Delete"
        pendingLabel="Deleting…"
        confirmPending={deleteGroup.isPending}
        allowCancelWhilePending
        onConfirm={() => {
          setDeleteError(null);
          if (deleteTarget) deleteGroup.mutate(deleteTarget.id);
        }}
      />
    </div>
  );
}

function ManageGroupDialog({ group, onClose }: { group: Group; onClose: () => void }) {
  const qc = useQueryClient();
  const { data: members = [] } = useQuery({
    queryKey: ["group-members", group.id],
    queryFn: () => api.listGroupMembers(group.id),
  });
  const { data: groupRoles = [] } = useQuery({
    queryKey: ["group-roles", group.id],
    queryFn: () => api.listGroupRoles(group.id),
  });
  const { data: dsAccess } = useQuery({
    queryKey: ["group-data-sources", group.id],
    queryFn: () => api.listGroupDataSources(group.id),
  });
  const restricted = dsAccess?.restricted ?? false;
  const entries = dsAccess?.entries ?? [];
  const { data: dataSourcesResp } = useQuery({ queryKey: ["data-sources"], queryFn: () => api.listDataSources() });
  const allDs = dataSourcesResp?.data_sources ?? [];
  const { data: allUsers = [] } = useQuery({ queryKey: ["users"], queryFn: () => api.listUsers() });

  const invalidateMembers = () => {
    qc.invalidateQueries({ queryKey: ["group-members", group.id] });
    qc.invalidateQueries({ queryKey: SESSION_QUERY_KEY });
  };
  const invalidateRoles = () => {
    qc.invalidateQueries({ queryKey: ["group-roles", group.id] });
    qc.invalidateQueries({ queryKey: ["groups"] });
    qc.invalidateQueries({ queryKey: SESSION_QUERY_KEY });
  };
  const invalidateDs = () => qc.invalidateQueries({ queryKey: ["group-data-sources", group.id] });

  // One field per section so a failed data-source toggle does not report next to Members.
  const [memberError, setMemberError] = useState<string | null>(null);
  const [roleError, setRoleError] = useState<string | null>(null);
  const [dsError, setDsError] = useState<string | null>(null);

  const sectionHandlers = (setError: (msg: string | null) => void, invalidate: () => void, failure: string) => ({
    onSuccess: () => {
      setError(null);
      invalidate();
    },
    onError: (err: unknown) => setError(apiErrorMessage(err, failure)),
  });
  const removeMember = useMutation({
    mutationFn: (userId: number) => api.removeGroupMember(group.id, userId),
    ...sectionHandlers(setMemberError, invalidateMembers, "Failed to remove member"),
  });
  const addMember = useMutation({
    mutationFn: (userId: number) => api.addGroupMember(group.id, userId),
    ...sectionHandlers(setMemberError, invalidateMembers, "Failed to add member"),
  });
  const removeRole = useMutation({
    mutationFn: (roleId: number) => api.removeGroupRole(group.id, roleId),
    ...sectionHandlers(setRoleError, invalidateRoles, "Failed to detach role"),
  });
  const addRole = useMutation({
    mutationFn: (roleId: number) => api.addGroupRole(group.id, roleId),
    ...sectionHandlers(setRoleError, invalidateRoles, "Failed to attach role"),
  });
  const removeDs = useMutation({
    mutationFn: (dsId: number) => api.removeGroupDataSource(group.id, dsId),
    ...sectionHandlers(setDsError, invalidateDs, "Failed to remove data source"),
  });
  const addDs = useMutation({
    mutationFn: (dsId: number) => api.assignGroupDataSource(group.id, { data_source_id: dsId }),
    ...sectionHandlers(setDsError, invalidateDs, "Failed to add data source"),
  });
  const setRestricted = useMutation({
    mutationFn: (next: boolean) => api.setGroupDataSourceRestricted(group.id, next),
    ...sectionHandlers(setDsError, invalidateDs, "Failed to update restricted setting"),
  });

  const memberIds = new Set(members.map((m) => m.id));
  const assignedDsIds = new Set(entries.map((a) => a.data_source_id));
  const assignedRoleIds = new Set(groupRoles.map((r) => r.id));
  const canManageAccess = groupRoles.some(
    (r) => r.permissions.includes("manage_data_source") || r.permissions.includes("admin"),
  );
  const showManageNote = restricted && canManageAccess;
  const restrictedToggleId = `ds-restricted-${group.id}`;
  const restrictedHelpId = `ds-restricted-help-${group.id}`;
  const manageWarningId = `ds-restricted-manage-warning-${group.id}`;

  return (
    <Dialog open onOpenChange={onClose}>
      <DialogContent className="max-h-[85dvh] max-w-2xl overflow-y-auto">
        <DialogHeader>
          <DialogTitle>Manage {group.name}</DialogTitle>
        </DialogHeader>
        <div className="space-y-6 pt-2">
          <div>
            <h3 className="text-sm font-medium">Members</h3>
            <ul className="mt-2 space-y-1">
              {members.map((m) => (
                <li key={m.id} className="flex items-center justify-between text-sm">
                  <span>
                    {m.name} ({m.email})
                  </span>
                  <button
                    type="button"
                    className="cursor-pointer text-muted-foreground hover:text-destructive"
                    aria-label={`Remove ${m.name}`}
                    title="Remove member"
                    onClick={() => removeMember.mutate(m.id)}
                  >
                    <X size={14} aria-hidden="true" />
                  </button>
                </li>
              ))}
              {members.length === 0 && <li className="text-sm text-muted-foreground">No members yet.</li>}
            </ul>
            {memberError && <p className="mt-2 text-sm text-destructive">{memberError}</p>}
            <div className="mt-2">
              <PickerPopover
                triggerLabel="Add member…"
                items={allUsers.filter((u) => !memberIds.has(u.id))}
                renderLabel={(u) => `${u.name} (${u.email})`}
                onSelect={(u) => addMember.mutate(u.id)}
              />
            </div>
          </div>

          <div>
            <h3 className="text-sm font-medium">Roles</h3>
            <p className="text-xs text-muted-foreground">
              Members get the combined permissions of every attached role.
            </p>
            <div className="mt-2 flex flex-wrap gap-1">
              {groupRoles.map((r) => (
                <Badge key={r.id} variant="outline" className="gap-1">
                  {r.name}
                  <button
                    type="button"
                    className="cursor-pointer"
                    aria-label={`Detach ${r.name}`}
                    title="Detach role"
                    onClick={() => removeRole.mutate(r.id)}
                  >
                    <X size={12} aria-hidden="true" />
                  </button>
                </Badge>
              ))}
              {groupRoles.length === 0 && <span className="text-sm text-muted-foreground">No roles attached.</span>}
            </div>
            {roleError && <p className="mt-2 text-sm text-destructive">{roleError}</p>}
            <div className="mt-2">
              <MultiSelectRoles
                assignedRoleIds={assignedRoleIds}
                onAdd={(id) => addRole.mutate(id)}
                onRemove={(id) => removeRole.mutate(id)}
                pending={addRole.isPending || removeRole.isPending}
              />
            </div>
          </div>

          <div>
            <h3 className="text-sm font-medium">Data sources</h3>
            <div className="mt-2 flex items-center gap-2">
              <Switch
                id={restrictedToggleId}
                checked={restricted}
                // `restricted` defaults to false until loaded; a click would write the opposite of
                // the group's real value.
                disabled={setRestricted.isPending || dsAccess === undefined}
                aria-describedby={showManageNote ? `${restrictedHelpId} ${manageWarningId}` : restrictedHelpId}
                onCheckedChange={(checked) => setRestricted.mutate(checked)}
              />
              <label htmlFor={restrictedToggleId} className="text-sm">
                Restricted
              </label>
            </div>
            {dsError && <p className="mt-2 text-sm text-destructive">{dsError}</p>}
            <p id={restrictedHelpId} className="mt-1 text-xs text-muted-foreground">
              {restricted
                ? "This group can see and query only the data sources listed below."
                : "This group can see and query all data sources."}
            </p>

            {!restricted && (
              <p className="mt-2 text-xs text-muted-foreground">
                Turn on Restricted to limit this group to specific data sources.
              </p>
            )}

            {restricted && (
              <>
                {showManageNote && (
                  <div
                    id={manageWarningId}
                    aria-live="polite"
                    className="mt-2 flex items-start gap-2 rounded-md border border-warning/40 bg-warning/10 p-3"
                  >
                    <AlertTriangle
                      className="size-4 shrink-0 mt-0.5 text-warning"
                      aria-hidden="true"
                    />
                    <div className="space-y-1">
                      <p className="text-sm font-medium text-warning">
                        This group can manage data sources — but only the ones you allow.
                      </p>
                      <p className="text-xs text-muted-foreground">
                        Restricted is on and an attached role grants Manage data sources, so
                        members can create, edit, pause, and delete only the data sources on the
                        list below — not others in the workspace. Turn off Restricted, or detach
                        that role, if you meant for this group to manage every data source.
                      </p>
                    </div>
                  </div>
                )}
                <ul className="mt-2 space-y-1">
                  {entries.map((a) => (
                    <li key={a.id} className="flex items-center justify-between text-sm">
                      <span>{a.name}</span>
                      <button
                        type="button"
                        className="cursor-pointer text-muted-foreground hover:text-destructive"
                        aria-label={`Remove ${a.name}`}
                        title="Remove data source"
                        onClick={() => removeDs.mutate(a.data_source_id)}
                      >
                        <X size={14} aria-hidden="true" />
                      </button>
                    </li>
                  ))}
                  {entries.length === 0 && (
                    <li className="text-sm text-muted-foreground">
                      Restricted, but the list is empty — this group can&apos;t see any data source
                      until you add one below.
                    </li>
                  )}
                </ul>
                <div className="mt-2">
                  <PickerPopover
                    triggerLabel="Add a data source…"
                    items={allDs.filter((d) => !assignedDsIds.has(d.id))}
                    renderLabel={(d) => d.name}
                    onSelect={(d) => addDs.mutate(d.id)}
                  />
                </div>
              </>
            )}
          </div>
        </div>
        <DialogFooter>
          <Button variant="outline" onClick={onClose}>
            Close
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
