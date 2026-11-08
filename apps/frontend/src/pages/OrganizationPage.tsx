import { useState, useEffect } from "react";
import { useQuery, useMutation, useQueryClient } from "@tanstack/react-query";
import { api, apiErrorMessage } from "@/lib/api";
import { PageLoader } from "@/components/ui/loader";
import { Switch } from "@/components/ui/switch";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Button } from "@/components/ui/button";
import { PageHeader } from "@/components/common/PageHeader";
import { ReadonlyField } from "@/components/common/ReadonlyField";
import { useDocumentTitle } from "@/lib/useDocumentTitle";

export default function OrganizationPage() {
  const qc = useQueryClient();
  useDocumentTitle("Organization · arcodash");
  const { data: org, isLoading } = useQuery({
    queryKey: ["organization"],
    queryFn: () => api.getOrganization(),
  });

  const [name, setName] = useState("");
  useEffect(() => {
    if (org) setName(org.name);
    // eslint-disable-next-line react-hooks/exhaustive-deps -- re-seed only when the saved name changes
  }, [org?.name]);

  const update = useMutation({
    mutationFn: () => api.updateOrganization({ name }),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["organization"] });
    },
  });

  if (isLoading) return <PageLoader />;
  if (!org) return <p className="p-6">Not available.</p>;

  return (
    <div className="max-w-2xl space-y-6">
      <PageHeader title="Organization Settings" />

      <Card>
        <CardContent className="pt-6">
          <form
            onSubmit={(e) => { e.preventDefault(); update.mutate(); }}
            className="space-y-4"
          >
            <div className="space-y-2">
              <Label htmlFor="org-name">Organization Name</Label>
              <Input id="org-name" value={name} onChange={(e) => setName(e.target.value)} />
            </div>

            <ReadonlyField
              id="org-slug"
              label="Slug"
              value={org.slug}
              hint="Used in URLs. Fixed once the organization is created."
            />

            <div className="flex items-center gap-3">
              <Button type="submit" disabled={update.isPending || !name.trim()}>
                {update.isPending ? "Saving…" : "Save"}
              </Button>
              {update.isSuccess && <span className="text-sm text-green-600">Saved.</span>}
              {update.isError && (
                <span className="text-sm text-destructive">{apiErrorMessage(update.error, "Failed to save")}</span>
              )}
            </div>
          </form>
        </CardContent>
      </Card>

      <PublicUrlsSection />
    </div>
  );
}

/** Org-wide "Public URLs" pause — a soft, reversible, non-destructive global kill-switch
 * layered on top of per-embed revoke/expiry. Turning it off never deletes or regenerates any
 * link; turning it back on restores every one at the same URL. */
function PublicUrlsSection() {
  const qc = useQueryClient();
  const { data: settings, isLoading } = useQuery({
    queryKey: ["org-embed-settings"],
    queryFn: () => api.getOrgEmbedSettings(),
  });

  const [toggleError, setToggleError] = useState<string | null>(null);
  const toggle = useMutation({
    mutationFn: (enabled: boolean) => api.setOrgEmbedSettings({ public_urls_enabled: enabled }),
    onSuccess: () => {
      setToggleError(null);
      qc.invalidateQueries({ queryKey: ["org-embed-settings"] });
    },
    onError: (err) => setToggleError(apiErrorMessage(err, "Failed to update Public URLs setting")),
  });

  return (
    <Card>
      <CardHeader>
        <CardTitle>Public URLs</CardTitle>
      </CardHeader>
      <CardContent className="space-y-2">
        {isLoading || !settings ? (
          <p className="text-sm text-muted-foreground">Loading…</p>
        ) : (
          <div className="flex items-center gap-3">
            <Switch
              checked={settings.public_urls_enabled}
              onCheckedChange={(checked) => toggle.mutate(checked)}
              disabled={toggle.isPending}
              aria-label="Public URLs"
            />
            <span className="text-sm">
              {settings.public_urls_enabled
                ? "On — public dashboard/embed links are reachable."
                : "Off — every public link in this workspace is paused."}
            </span>
          </div>
        )}
        <p className="text-sm text-muted-foreground">
          Pausing is one click and reversible: no link is deleted or regenerated. Turning this
          back on restores every existing public link at the exact same URL.
        </p>
        {toggleError && <p className="text-sm text-destructive">{toggleError}</p>}
      </CardContent>
    </Card>
  );
}
