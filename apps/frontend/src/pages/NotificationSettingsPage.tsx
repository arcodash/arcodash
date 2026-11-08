import { useEffect, useMemo, useState } from "react";
import { useMutation, useQueryClient } from "@tanstack/react-query";
import { Card, CardContent } from "@/components/ui/card";
import { Label } from "@/components/ui/label";
import { Switch } from "@/components/ui/switch";
import {
  Select,
  SelectContent,
  SelectGroup,
  SelectItem,
  SelectLabel,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { PageHeader } from "@/components/common/PageHeader";
import { BackButton } from "@/components/nav/BackButton";
import { PageLoader } from "@/components/ui/loader";
import { api } from "@/lib/api";
import { useAuth, SESSION_QUERY_KEY } from "@/lib/auth";

const HOUR_OPTIONS = Array.from({ length: 24 }, (_, h) => h);

function formatHour(hour: number): string {
  return `${String(hour).padStart(2, "0")}:00`;
}

interface DigestSave {
  enabled: boolean;
  hour?: number;
  timezone?: string;
}

/** Local editable copy of the digest settings, seeded from the session. `hasStoredHour` is whether the server has
 * recorded an hour/timezone, distinct from `enabled` (disabling keeps the schedule preference). */
interface DigestState {
  enabled: boolean;
  hour: number;
  timezone: string;
  hasStoredHour: boolean;
}

export default function NotificationSettingsPage() {
  const queryClient = useQueryClient();
  const { user, isLoading, clientConfig } = useAuth();

  // Computed at render time (not module scope) so tests can mock `Intl` before rendering.
  const detectedTimezone = useMemo(() => Intl.DateTimeFormat().resolvedOptions().timeZone, []);
  const allTimezones = useMemo(() => {
    if (typeof Intl.supportedValuesOf === "function") {
      return [...Intl.supportedValuesOf("timeZone")].sort();
    }
    return [detectedTimezone, "UTC"];
  }, [detectedTimezone]);
  // Excludes the detected zone: two SelectItems sharing a value double the SelectValue label (Radix
  // resolves the trigger text from every item with that value).
  const otherTimezones = useMemo(
    () => allTimezones.filter((tz) => tz !== detectedTimezone),
    [allTimezones, detectedTimezone],
  );

  const briefs = clientConfig?.ai_dashboard_narratives === true;
  const [state, setState] = useState<DigestState | null>(null);
  const [saveMessage, setSaveMessage] = useState<{ kind: "success" | "error"; text: string } | null>(null);

  // Seeds `state` once when the session first loads; a `useState` initializer would run before the
  // fetch resolves and lock in wrong values.
  useEffect(() => {
    if (isLoading || state !== null) return;
    const storedHour = typeof user?.digest_hour === "number" ? user.digest_hour : null;
    const storedTimezone = typeof user?.digest_timezone === "string" ? user.digest_timezone : null;
    setState({
      enabled: Boolean(user?.digest_enabled ?? false),
      hour: storedHour ?? 8,
      timezone: storedTimezone ?? detectedTimezone,
      hasStoredHour: storedHour != null,
    });
  }, [isLoading, state, user, detectedTimezone]);

  const mutation = useMutation({
    mutationFn: (body: DigestSave) => api.setDigest(body),
    onMutate: (body) => {
      if (!state) return undefined;
      const prev = state;
      setSaveMessage(null);
      setState({
        enabled: body.enabled,
        hour: body.hour ?? state.hour,
        timezone: body.timezone ?? state.timezone,
        hasStoredHour: state.hasStoredHour || body.hour !== undefined,
      });
      return prev;
    },
    onSuccess: (_result, body) => {
      void queryClient.invalidateQueries({ queryKey: SESSION_QUERY_KEY });
      setSaveMessage({
        kind: "success",
        text: body.enabled ? "Saved." : "Saved. You won't get the daily digest.",
      });
    },
    onError: (_err, _body, prev) => {
      if (prev) setState(prev);
      setSaveMessage({ kind: "error", text: "Couldn't save. Try again." });
    },
  });

  if (isLoading || state === null) return <PageLoader />;

  const isDetectedTimezone = state.timezone === detectedTimezone;

  return (
    <div className="space-y-6">
      <BackButton to="/settings" label="Back to settings" />
      <PageHeader title="Notifications" />

      <Card className="max-w-2xl">
        <CardContent className="space-y-4 pt-6">
          <div className="flex items-start justify-between gap-6">
            <div className="space-y-1">
              <Label htmlFor="digest-toggle" className="text-sm font-medium">
                {briefs ? "Email me a daily digest" : "Email me a daily digest of my monitors"}
              </Label>
              <p className="text-sm text-muted-foreground">
                {briefs
                  ? "A summary of the last 24 hours for the monitors you follow. Nothing to report means no email. Every digest has a one-click unsubscribe link."
                  : "A summary of the last 24 hours of changes for the monitors you follow. Nothing changed means no email. Every digest has a one-click unsubscribe link."}
              </p>
              {briefs && (
                <p className="text-sm text-muted-foreground">
                  Includes an AI brief for up to 3 of your favorite dashboards.
                </p>
              )}
            </div>
            <Switch
              id="digest-toggle"
              checked={state.enabled}
              disabled={mutation.isPending}
              onCheckedChange={(next) => {
                if (next && !state.hasStoredHour) {
                  mutation.mutate({ enabled: true, hour: state.hour, timezone: state.timezone });
                } else {
                  mutation.mutate({ enabled: next });
                }
              }}
            />
          </div>

          {state.enabled && (
            <div className="space-y-3 border-t pt-4">
              <div className="flex flex-wrap items-end gap-4">
                <div className="space-y-1">
                  <Label htmlFor="digest-hour">Send at</Label>
                  <Select
                    value={String(state.hour)}
                    onValueChange={(v) => mutation.mutate({ enabled: true, hour: Number(v) })}
                    disabled={mutation.isPending}
                  >
                    <SelectTrigger id="digest-hour" className="w-28">
                      <SelectValue />
                    </SelectTrigger>
                    <SelectContent>
                      {HOUR_OPTIONS.map((h) => (
                        <SelectItem key={h} value={String(h)}>
                          {formatHour(h)}
                        </SelectItem>
                      ))}
                    </SelectContent>
                  </Select>
                </div>

                <div className="space-y-1">
                  <Label htmlFor="digest-timezone">Timezone</Label>
                  <Select
                    value={state.timezone}
                    onValueChange={(v) => mutation.mutate({ enabled: true, timezone: v })}
                    disabled={mutation.isPending}
                  >
                    <SelectTrigger
                      id="digest-timezone"
                      className="w-full sm:w-72"
                      aria-describedby={isDetectedTimezone ? "digest-tz-hint" : undefined}
                    >
                      <SelectValue />
                    </SelectTrigger>
                    <SelectContent>
                      <SelectGroup>
                        <SelectLabel>Detected</SelectLabel>
                        <SelectItem value={detectedTimezone}>{detectedTimezone}</SelectItem>
                      </SelectGroup>
                      <SelectGroup>
                        <SelectLabel>All timezones</SelectLabel>
                        {otherTimezones.map((tz) => (
                          <SelectItem key={tz} value={tz}>
                            {tz}
                          </SelectItem>
                        ))}
                      </SelectGroup>
                    </SelectContent>
                  </Select>
                  {isDetectedTimezone && (
                    <p id="digest-tz-hint" className="text-sm text-muted-foreground">
                      Detected from your browser.
                    </p>
                  )}
                </div>
              </div>

              <p className="text-sm text-muted-foreground">
                You'll get your digest at {formatHour(state.hour)} each day, {state.timezone} time.
              </p>
            </div>
          )}

          {saveMessage && (
            <p className={`text-sm ${saveMessage.kind === "success" ? "text-green-600" : "text-destructive"}`}>
              {saveMessage.text}
            </p>
          )}
        </CardContent>
      </Card>
    </div>
  );
}
