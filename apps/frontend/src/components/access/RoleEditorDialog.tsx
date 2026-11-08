import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { ApiError, api } from "@/lib/api";
import type { Permission, Role } from "@/types/api";
import { Dialog, DialogContent, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Button } from "@/components/ui/button";
import { Loader } from "@/components/ui/loader";
import { SESSION_QUERY_KEY } from "@/lib/auth";

/** role === null → create mode; role.type === "builtin" → read-only (locked) view;
 * `canManage=false` → read-only regardless of type (viewer with `view_roles` only). */
export function RoleEditorDialog({
  role,
  canManage = true,
  onClose,
}: {
  role: Role | null;
  canManage?: boolean;
  onClose: () => void;
}) {
  const qc = useQueryClient();
  const readOnly = role?.type === "builtin" || !canManage;
  const { data: catalog = [] } = useQuery({ queryKey: ["permissions"], queryFn: () => api.listPermissions() });
  const [name, setName] = useState(role?.name ?? "");
  const [perms, setPerms] = useState<Set<string>>(new Set(role?.permissions ?? []));
  const [error, setError] = useState<string | null>(null);

  const save = useMutation({
    mutationFn: () => {
      const body = { name, permissions: [...perms] };
      return role ? api.updateRole(role.id, body) : api.createRole(body);
    },
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["roles"] });
      qc.invalidateQueries({ queryKey: SESSION_QUERY_KEY });
      onClose();
    },
    onError: (err) => setError(err instanceof ApiError ? err.message : "Failed to save role"),
  });

  // Group by category in first-seen order so the layout stays stable if the catalog grows.
  const grouped = new Map<string, Permission[]>();
  for (const p of catalog) {
    const list = grouped.get(p.category);
    if (list) list.push(p);
    else grouped.set(p.category, [p]);
  }

  function togglePermission(key: string, checked: boolean) {
    setPerms((current) => {
      const next = new Set(current);
      if (checked) next.add(key);
      else next.delete(key);
      return next;
    });
  }

  return (
    <Dialog open onOpenChange={onClose}>
      <DialogContent className="max-h-[85dvh] max-w-lg overflow-y-auto">
        <DialogHeader>
          <DialogTitle>{role ? (readOnly ? role.name : "Edit role") : "New role"}</DialogTitle>
        </DialogHeader>
        <div className="space-y-4 pt-2">
          {role?.type === "builtin" && (
            <p className="rounded-md bg-muted px-3 py-2 text-sm text-muted-foreground">
              Built-in role. Permissions are fixed and can't be changed.
            </p>
          )}
          <div>
            <label className="text-sm font-medium" htmlFor="role-name">
              Name
            </label>
            <Input
              id="role-name"
              value={name}
              disabled={readOnly}
              onChange={(e) => setName(e.target.value)}
              placeholder="Role name"
            />
          </div>

          {[...grouped].map(([cat, items]) => (
            <div key={cat}>
              <p className="mb-1 text-xs font-medium uppercase text-muted-foreground">{cat}</p>
              {cat === "Admin" && (
                <p className="mb-1 text-xs text-muted-foreground">Grants full workspace access.</p>
              )}
              <div className="grid grid-cols-2 gap-1">
                {items.map((p) => (
                  <label key={p.key} className="flex cursor-pointer items-center gap-2 text-sm">
                    <input
                      type="checkbox"
                      className="size-4 cursor-pointer accent-primary"
                      disabled={readOnly}
                      checked={perms.has(p.key)}
                      onChange={(e) => togglePermission(p.key, e.target.checked)}
                    />
                    {p.label}
                  </label>
                ))}
              </div>
            </div>
          ))}

          {error && <p className="text-sm text-destructive">{error}</p>}
        </div>
        <DialogFooter>
          <Button variant="outline" onClick={onClose}>
            {readOnly ? "Close" : "Cancel"}
          </Button>
          {!readOnly && (
            <Button
              disabled={!name.trim() || save.isPending}
              onClick={() => {
                setError(null);
                save.mutate();
              }}
            >
              {save.isPending ? (
                <>
                  <Loader size={16} />
                  Saving…
                </>
              ) : (
                "Save"
              )}
            </Button>
          )}
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
