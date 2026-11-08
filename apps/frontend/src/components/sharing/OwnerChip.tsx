import { Avatar, AvatarFallback } from "@/components/ui/avatar";

interface OwnerChipProps {
  name: string;
  isCurrentUser?: boolean;
  /** List rows: true. Tight card corners: false (avatar-only + title tooltip). */
  showName?: boolean;
}

export function initials(name: string): string {
  const parts = name.trim().split(/\s+/).filter(Boolean);
  if (parts.length === 0) return "?";
  return parts
    .slice(0, 2)
    .map((p) => p[0]?.toUpperCase())
    .join("");
}

/** Owner identity at list-row/card density. Avatars show initials only
 * (`listUsers` returns a name, no image). */
export function OwnerChip({ name, isCurrentUser, showName }: OwnerChipProps) {
  return (
    <span className="inline-flex items-center gap-1.5" title={name}>
      <Avatar className="size-5">
        <AvatarFallback className="text-[10px]">{initials(name)}</AvatarFallback>
      </Avatar>
      {showName && (
        <span className="truncate text-xs text-muted-foreground">
          {isCurrentUser ? "You" : name}
        </span>
      )}
    </span>
  );
}
