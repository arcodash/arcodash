import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { AlertTriangle, Lock, ShieldCheck } from "lucide-react";
import { api, apiErrorMessage } from "@/lib/api";
import type { Role } from "@/types/api";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { Dialog, DialogContent, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog";
import { PageLoader } from "@/components/ui/loader";
import { EmptyState } from "@/components/common/EmptyState";
import { RoleEditorDialog } from "@/components/access/RoleEditorDialog";
import { usePermissions } from "@/lib/permissions";
import { SESSION_QUERY_KEY } from "@/lib/auth";

function permissionCountLabel(role: Role) {
  // The "admin" key is a wildcard that outranks every other permission — a bare count of 1
  // would read as weaker than roles with many individually-listed permissions.
  if (role.permissions.includes("admin")) return "All permissions";
  const n = role.permissions.length;
  return `${n} permission${n === 1 ? "" : "s"}`;
}

function grantsNothingButAssigned(role: Role) {
  return role.permissions.length === 0 && role.group_count > 0;
}

function usedByLabel(role: Role) {
  return role.group_count > 0 ? `${role.group_count} group${role.group_count === 1 ? "" : "s"}` : "Not used";
}

/** Roles tab panel — no outer PageHeader, nests under the Access Control shell. */
export default function RolesPage() {
  const qc = useQueryClient();
  const { has } = usePermissions();
  const canManage = has("manage_roles");
  const rolesQuery = useQuery({ queryKey: ["roles"], queryFn: () => api.listRoles() });

  // undefined = editor closed, null = create mode, Role = edit/view mode.
  const [editorRole, setEditorRole] = useState<Role | null | undefined>(undefined);
  const [deleteTarget, setDeleteTarget] = useState<Role | null>(null);
  const [deleteError, setDeleteError] = useState<string | null>(null);

  const deleteRole = useMutation({
    mutationFn: (id: number) => api.deleteRole(id),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["roles"] });
      qc.invalidateQueries({ queryKey: SESSION_QUERY_KEY });
      setDeleteTarget(null);
    },
    onError: (err) => setDeleteError(apiErrorMessage(err, "Failed to delete role")),
  });

  if (rolesQuery.isError) {
    return <p className="text-destructive">Failed to load roles.</p>;
  }

  function closeDelete() {
    setDeleteTarget(null);
    setDeleteError(null);
  }

  const roles = rolesQuery.data ?? [];
  const hasCustomRole = roles.some((r) => r.type !== "builtin");
  const blockedDelete = (deleteTarget?.group_count ?? 0) > 0;

  return (
    <div className="space-y-4">
      {canManage && (
        <div className="flex items-center justify-end">
          <Button onClick={() => setEditorRole(null)}>+ New role</Button>
        </div>
      )}

      {rolesQuery.isLoading && <PageLoader />}

      {!rolesQuery.isLoading && roles.length === 0 && (
        <EmptyState
          icon={ShieldCheck}
          title="No custom roles yet."
          description="Built-in roles cover the common cases; create a custom role for finer-grained access."
        />
      )}

      {roles.length > 0 && (
        <>
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead>Name</TableHead>
                <TableHead>Type</TableHead>
                <TableHead>Permissions</TableHead>
                <TableHead>Used by</TableHead>
                <TableHead>Actions</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {roles.map((r) => {
                const builtin = r.type === "builtin";
                return (
                  <TableRow key={r.id}>
                    <TableCell>
                      <Button variant="link" className="h-auto p-0" onClick={() => setEditorRole(r)}>
                        {r.name}
                      </Button>
                    </TableCell>
                    <TableCell>
                      <Badge variant={builtin ? "secondary" : "outline"}>{builtin ? "Built-in" : "Custom"}</Badge>
                    </TableCell>
                    <TableCell className="text-muted-foreground">
                      <span className="flex items-center gap-1.5">
                        {permissionCountLabel(r)}
                        {grantsNothingButAssigned(r) && (
                          <span
                            aria-label="Assigned to a group but grants no permissions"
                            title="Assigned to a group but grants no permissions"
                          >
                            <AlertTriangle size={14} className="text-warning" aria-hidden="true" />
                          </span>
                        )}
                      </span>
                    </TableCell>
                    <TableCell className="text-muted-foreground">{usedByLabel(r)}</TableCell>
                    <TableCell>
                      {builtin || !canManage ? (
                        <div className="flex items-center gap-1">
                          <Button variant="ghost" size="sm" onClick={() => setEditorRole(r)}>
                            View
                          </Button>
                          {builtin && (
                            <span
                              aria-label="Built-in roles can't be edited"
                              title="Built-in roles can't be edited"
                            >
                              <Lock size={14} className="text-muted-foreground" aria-hidden="true" />
                            </span>
                          )}
                        </div>
                      ) : (
                        <div className="flex gap-1">
                          <Button variant="ghost" size="sm" onClick={() => setEditorRole(r)}>
                            Edit
                          </Button>
                          <Button
                            variant="ghost"
                            size="sm"
                            className="text-destructive"
                            onClick={() => {
                              setDeleteError(null);
                              setDeleteTarget(r);
                            }}
                          >
                            Delete
                          </Button>
                        </div>
                      )}
                    </TableCell>
                  </TableRow>
                );
              })}
            </TableBody>
          </Table>
          {!hasCustomRole && (
            <p className="text-sm text-muted-foreground">
              No custom roles yet. Built-in roles cover the common cases; create a custom role for
              finer-grained access.
            </p>
          )}
        </>
      )}

      {editorRole !== undefined && (
        <RoleEditorDialog role={editorRole} canManage={canManage} onClose={() => setEditorRole(undefined)} />
      )}

      <Dialog
        open={deleteTarget !== null}
        onOpenChange={(o) => !o && closeDelete()}
      >
        <DialogContent>
          <DialogHeader>
            <DialogTitle>Delete role &quot;{deleteTarget?.name}&quot;?</DialogTitle>
          </DialogHeader>
          <p className="text-sm text-muted-foreground">
            {blockedDelete
              ? `This role is used by ${deleteTarget?.group_count} group${deleteTarget?.group_count === 1 ? "" : "s"} and can't be deleted until it's detached from them.`
              : "This role will be removed. Users in groups that use it will lose the permissions it grants."}
          </p>
          {deleteError && <p className="text-sm text-destructive">{deleteError}</p>}
          <DialogFooter>
            <Button variant="outline" onClick={closeDelete}>
              Cancel
            </Button>
            <Button
              variant="destructive"
              disabled={deleteRole.isPending || blockedDelete}
              title={blockedDelete ? "Detach this role from every group before deleting it" : undefined}
              onClick={() => {
                setDeleteError(null);
                if (deleteTarget) deleteRole.mutate(deleteTarget.id);
              }}
            >
              {deleteRole.isPending ? "Deleting…" : "Delete"}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </div>
  );
}
