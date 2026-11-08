import { ArrowDown, ArrowUp } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Switch } from "@/components/ui/switch";
import { Badge } from "@/components/ui/badge";
import { ProviderStatusBadge } from "@/components/ai/ProviderStatusBadge";
import type { AiProviderInfo } from "@/types/api";

interface ProviderPriorityCardProps {
  /** Every currently-keyed provider kind, in priority order — `[0]` is always primary. */
  orderedKinds: string[];
  labels: Record<string, string>;
  providers: Record<string, AiProviderInfo>;
  fallbackEnabled: boolean;
  saving: boolean;
  onReorder: (newOrder: string[]) => void;
  onToggleFallback: (enabled: boolean) => void;
}

/** One ordered list doubling as both "who's active" (row 1) and the fallback chain (rows
 * 2+) — no separate active-provider selector. Only providers with a saved key are eligible. */
export function ProviderPriorityCard({
  orderedKinds,
  labels,
  providers,
  fallbackEnabled,
  saving,
  onReorder,
  onToggleFallback,
}: ProviderPriorityCardProps) {
  if (orderedKinds.length === 0) return null;

  const swap = (i: number, j: number) => {
    const next = [...orderedKinds];
    [next[i], next[j]] = [next[j], next[i]];
    onReorder(next);
  };

  return (
    <Card>
      <CardHeader>
        <CardTitle className="flex items-center justify-between gap-2">
          <span>Provider priority</span>
          {orderedKinds.length > 1 && (
            <label className="flex items-center gap-2 text-sm font-normal text-muted-foreground">
              Enable fallback
              <Switch checked={fallbackEnabled} disabled={saving} onCheckedChange={onToggleFallback} />
            </label>
          )}
        </CardTitle>
      </CardHeader>
      <CardContent className="space-y-2">
        {orderedKinds.map((kind, i) => {
          const dimmed = i > 0 && !fallbackEnabled;
          const label = labels[kind] ?? kind;
          return (
            <div
              key={kind}
              className={`flex items-center gap-3 rounded-md border px-3 py-2 ${dimmed ? "opacity-50" : ""}`}
            >
              <Badge variant={i === 0 ? "default" : "outline"} className="w-16 justify-center">
                {i === 0 ? "Primary" : i + 1}
              </Badge>
              <span className="flex-1 font-medium">{label}</span>
              <ProviderStatusBadge status={providers[kind]?.last_status ?? null} />
              {i > 0 && (
                <div className="flex items-center gap-1">
                  <Button
                    size="icon-sm"
                    variant="ghost"
                    aria-label={`Move ${label} up`}
                    disabled={i === 1 || saving}
                    onClick={() => swap(i, i - 1)}
                  >
                    <ArrowUp size={16} />
                  </Button>
                  <Button
                    size="icon-sm"
                    variant="ghost"
                    aria-label={`Move ${label} down`}
                    disabled={i === orderedKinds.length - 1 || saving}
                    onClick={() => swap(i, i + 1)}
                  >
                    <ArrowDown size={16} />
                  </Button>
                </div>
              )}
            </div>
          );
        })}

        {orderedKinds.length === 1 ? (
          <p className="text-sm text-muted-foreground">
            Add and save a second provider key to enable fallback.
          </p>
        ) : fallbackEnabled ? (
          <p className="rounded-md border border-warning/30 bg-warning/10 px-3 py-2 text-sm text-warning-foreground">
            ⚠ When a fallback runs, that request is billed to the fallback provider&apos;s own API
            key — not your primary provider. Only add providers you&apos;re willing to be billed on.
          </p>
        ) : (
          <p className="text-sm text-muted-foreground">Enable fallback to use these.</p>
        )}
      </CardContent>
    </Card>
  );
}
