import { useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { api } from "@/lib/api";
import { matchesSearch } from "@/lib/utils";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import { Input } from "@/components/ui/input";
import { Button } from "@/components/ui/button";

/** Searchable popover picker of every org role, checked = assigned to the current group. */
export function MultiSelectRoles({
  assignedRoleIds,
  onAdd,
  onRemove,
  pending,
}: {
  assignedRoleIds: Set<number>;
  onAdd: (roleId: number) => void;
  onRemove: (roleId: number) => void;
  /** Disables every checkbox while an add/remove is in flight, so rapid clicks don't fan out
   * overlapping requests that can land out of order. */
  pending?: boolean;
}) {
  const { data: roles = [] } = useQuery({ queryKey: ["roles"], queryFn: () => api.listRoles() });
  const [open, setOpen] = useState(false);
  const [filter, setFilter] = useState("");

  const candidates = roles.filter((r) => matchesSearch(filter, r.name));

  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger asChild>
        <Button type="button" variant="outline" size="sm">
          Attach role…
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
          {candidates.length === 0 && <p className="px-1 py-2 text-sm text-muted-foreground">No roles found.</p>}
          {candidates.map((r) => {
            const checked = assignedRoleIds.has(r.id);
            return (
              <label
                key={r.id}
                className="flex cursor-pointer items-center gap-2 rounded px-1 py-1 text-sm hover:bg-muted"
              >
                <input
                  type="checkbox"
                  className="size-4 cursor-pointer accent-primary"
                  checked={checked}
                  disabled={pending}
                  onChange={(e) => (e.target.checked ? onAdd(r.id) : onRemove(r.id))}
                />
                {r.name}
              </label>
            );
          })}
        </div>
      </PopoverContent>
    </Popover>
  );
}
