import { CheckCircle2, Lock, PencilLine, Users, type LucideIcon } from "lucide-react";
import { Badge } from "@/components/ui/badge";
import type { ObjectType, Visibility } from "@/types/api";

interface VisibilityChipProps {
  visibility: Visibility;
  isDraft?: boolean;
  objectType: ObjectType;
}

/** Draft or Published badge; who can reach a published object is a Share dialog concern. Monitors have no draft
 * lifecycle (callers omit `isDraft`), so they show the reach tier instead. */
export function VisibilityChip({ visibility, isDraft }: VisibilityChipProps) {
  const [Icon, label]: [LucideIcon, string] =
    isDraft === undefined
      ? visibility === "restricted"
        ? [Lock, "Specific people"]
        : [Users, "Everyone"]
      : isDraft
        ? [PencilLine, "Draft"]
        : [CheckCircle2, "Published"];
  return (
    <Badge variant="outline" className={isDraft ? "gap-1 text-muted-foreground" : "gap-1"}>
      <Icon className="size-3" />
      {label}
    </Badge>
  );
}
