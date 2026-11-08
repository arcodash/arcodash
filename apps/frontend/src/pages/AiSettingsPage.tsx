import { useEffect, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Switch } from "@/components/ui/switch";
import { Label } from "@/components/ui/label";
import { Separator } from "@/components/ui/separator";
import { ConfirmDialog } from "@/components/ui/confirm-dialog";
import { AiTabs } from "@/components/ai/AiTabs";
import { ProviderCard, type ProviderSaveBody } from "@/components/ai/ProviderCard";
import { ProviderPriorityCard } from "@/components/ai/ProviderPriorityCard";
import { PageHeader } from "@/components/common/PageHeader";
import { DocsLink } from "@/components/common/DocsLink";
import { useTransientMessage } from "@/hooks/useTransientMessage";
import { api, apiErrorMessage } from "@/lib/api";
import { useAuth } from "@/lib/auth";
import { useDocumentTitle } from "@/lib/useDocumentTitle";
import { PageLoader } from "@/components/ui/loader";
import { formatUsd } from "@/lib/formatNumber";
import type { AiSettings, TestKeyResult } from "@/types/api";

const PROVIDER_KINDS = ["gemini", "openai", "anthropic"] as const;
const PROVIDER_LABELS: Record<string, string> = {
  gemini: "Gemini",
  openai: "OpenAI",
  anthropic: "Anthropic",
};
const PROVIDER_KEY_HELP: Record<string, string> = {
  gemini: "Get a key at aistudio.google.com.",
  openai: "Get a key at platform.openai.com.",
  anthropic: "Get a key at console.anthropic.com.",
};

type SaveMessage = { kind: "success" | "error"; text: string };

function SaveStatus({ message }: { message: SaveMessage | null }) {
  if (!message) return null;
  return (
    <span aria-live="polite" className={`text-sm ${message.kind === "success" ? "text-success" : "text-destructive"}`}>
      {message.text}
    </span>
  );
}

export default function AiSettingsPage() {
  useDocumentTitle("Providers · arcodash");
  const queryClient = useQueryClient();
  const { refreshSession } = useAuth();
  const { data, isLoading, isError, error } = useQuery({
    queryKey: ["ai-settings"],
    queryFn: () => api.getAiSettings(),
  });

  // Same key as the Usage tab's default so the cache is shared; a failure just hides the spend
  // line.
  const { data: usage } = useQuery({
    queryKey: ["ai-usage", 30],
    queryFn: () => api.getAiUsage(30),
  });

  const [rateLimitInput, setRateLimitInput] = useState("");
  const [budgetInput, setBudgetInput] = useState("");
  const [budgetMessage, setBudgetMessage] = useTransientMessage<SaveMessage>();
  const [rateLimitMessage, setRateLimitMessage] = useTransientMessage<SaveMessage>();
  const [testResults, setTestResults] = useState<Record<string, TestKeyResult | null>>({});
  const [saveMessages, setSaveMessages] = useState<Record<string, SaveMessage | null>>({});
  const [removeDialogKind, setRemoveDialogKind] = useState<string | null>(null);
  const [removeError, setRemoveError] = useState<string | null>(null);
  const [priorityError, setPriorityError] = useState<string | null>(null);

  // Re-seed only when the stored override changes: every mutation replaces the whole settings
  // object, and keying on it would wipe a number mid-typing.
  useEffect(() => {
    if (data) {
      setRateLimitInput(data.rate_limit_org_value != null ? String(data.rate_limit_org_value) : "");
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [data?.rate_limit_org_value]);

  useEffect(() => {
    if (data) setBudgetInput(data.monthly_budget_usd != null ? String(data.monthly_budget_usd) : "");
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [data?.monthly_budget_usd]);

  const saveMutation = useMutation({
    mutationFn: ({ kind, body }: { kind: string; body: ProviderSaveBody }) =>
      api.updateAiSettings({ providers: { [kind]: body } }),
    onSuccess: (updated, { kind, body }) => {
      queryClient.setQueryData(["ai-settings"], updated);
      void refreshSession();
      setTestResults((prev) => ({ ...prev, [kind]: null }));
      setSaveMessages((prev) => ({
        ...prev,
        [kind]: { kind: "success", text: body.api_key ? "✓ Saved. Ask with AI can now use this key." : "Saved." },
      }));
    },
    onError: (err, { kind }) => {
      setSaveMessages((prev) => ({ ...prev, [kind]: { kind: "error", text: apiErrorMessage(err, "Failed to save.") } }));
    },
  });

  const testMutation = useMutation({
    mutationFn: ({ kind, apiKey }: { kind: string; apiKey?: string }) =>
      api.testAiKey({ provider: kind, api_key: apiKey }),
    onSuccess: (result, { kind }) => setTestResults((prev) => ({ ...prev, [kind]: result })),
    onError: (err, { kind }) =>
      setTestResults((prev) => ({
        ...prev,
        [kind]: { status: "network_error", message: apiErrorMessage(err, "Failed to test key.") },
      })),
  });

  const closeRemoveDialog = () => {
    setRemoveDialogKind(null);
    setRemoveError(null);
  };

  const removeMutation = useMutation({
    mutationFn: (kind: string) => api.updateAiSettings({ providers: { [kind]: { api_key: null } } }),
    onSuccess: (updated) => {
      queryClient.setQueryData(["ai-settings"], updated);
      void refreshSession();
      closeRemoveDialog();
    },
    onError: (err) => setRemoveError(apiErrorMessage(err, "Failed to remove key.")),
  });

  // The priority card and the "Data sent to AI" toggles share one error line.
  const settingHandlers = (failure: string) => ({
    onSuccess: (updated: AiSettings) => {
      queryClient.setQueryData(["ai-settings"], updated);
      setPriorityError(null);
    },
    onError: (err: unknown) => setPriorityError(apiErrorMessage(err, failure)),
  });
  const orderMutation = useMutation({
    mutationFn: (provider_order: string[]) => api.updateAiSettings({ provider_order }),
    ...settingHandlers("Failed to reorder providers."),
  });
  const fallbackMutation = useMutation({
    mutationFn: (fallback_enabled: boolean) => api.updateAiSettings({ fallback_enabled }),
    ...settingHandlers("Failed to update fallback setting."),
  });
  const sampleValuesMutation = useMutation({
    mutationFn: (include_sample_values: boolean) => api.updateAiSettings({ include_sample_values }),
    ...settingHandlers("Failed to update sample values setting."),
  });
  const explainMutation = useMutation({
    mutationFn: (explain_alerts: boolean) => api.updateAiSettings({ explain_alerts }),
    ...settingHandlers("Failed to update alert explanations setting."),
  });
  const narrativesMutation = useMutation({
    mutationFn: (dashboard_narratives: boolean) => api.updateAiSettings({ dashboard_narratives }),
    ...settingHandlers("Failed to update dashboard explanations setting."),
  });

  const rateLimitChanged =
    data != null &&
    (rateLimitInput.trim() === ""
      ? data.rate_limit_org_value != null
      : Number(rateLimitInput.trim()) !== data.rate_limit_org_value);

  const rateLimitMutation = useMutation({
    mutationFn: () =>
      api.updateAiSettings({
        rate_limit_per_min: rateLimitInput.trim() === "" ? null : Math.trunc(Number(rateLimitInput.trim())),
      }),
    onSuccess: (updated) => {
      queryClient.setQueryData(["ai-settings"], updated);
      setRateLimitMessage({ kind: "success", text: "Saved." });
    },
    onError: (err) => setRateLimitMessage({ kind: "error", text: apiErrorMessage(err, "Failed to save.") }),
  });

  const budgetTrimmed = budgetInput.trim();
  const budgetNumber = Number(budgetTrimmed);
  const budgetInvalid = budgetTrimmed !== "" && !(Number.isFinite(budgetNumber) && budgetNumber > 0);
  const budgetChanged =
    data != null &&
    (budgetTrimmed === "" ? data.monthly_budget_usd != null : budgetNumber !== data.monthly_budget_usd);

  const budgetMutation = useMutation({
    mutationFn: () => api.updateAiSettings({ monthly_budget_usd: budgetTrimmed === "" ? null : budgetNumber }),
    onSuccess: (updated) => {
      queryClient.setQueryData(["ai-settings"], updated);
      void queryClient.invalidateQueries({ queryKey: ["ai-usage"] });
      setBudgetMessage({ kind: "success", text: "Saved." });
    },
    onError: (err) => setBudgetMessage({ kind: "error", text: apiErrorMessage(err, "Failed to save.") }),
  });

  if (isLoading) return <PageLoader />;
  if (isError || !data) {
    return <p className="text-destructive">{apiErrorMessage(error, "Failed to load AI settings")}</p>;
  }

  // Every keyed provider in priority order: resolved `provider_order` first, then unplaced keyed
  // providers, so reordering works before any order is saved.
  const keyedKinds = PROVIDER_KINDS.filter((k) => data.providers[k]?.key_configured);
  const orderedKinds = [
    ...data.provider_order.filter((k): k is (typeof PROVIDER_KINDS)[number] =>
      (keyedKinds as readonly string[]).includes(k),
    ),
    ...keyedKinds.filter((k) => !data.provider_order.includes(k)),
  ];

  return (
    <div className="max-w-2xl space-y-6">
      <PageHeader title="AI Settings" />
      <AiTabs />

      {PROVIDER_KINDS.map((kind) => (
        <ProviderCard
          key={kind}
          kind={kind}
          label={PROVIDER_LABELS[kind]}
          info={data.providers[kind]}
          keyHelp={PROVIDER_KEY_HELP[kind]}
          saving={saveMutation.isPending && saveMutation.variables?.kind === kind}
          saveMessage={saveMessages[kind]}
          onSave={(k, body) => saveMutation.mutate({ kind: k, body })}
          testing={testMutation.isPending && testMutation.variables?.kind === kind}
          testResult={testResults[kind] ?? null}
          onTest={(k, apiKey) => testMutation.mutate({ kind: k, apiKey })}
          onRequestRemove={(kind) => {
            setRemoveError(null);
            setRemoveDialogKind(kind);
          }}
        />
      ))}

      <ProviderPriorityCard
        orderedKinds={orderedKinds}
        labels={PROVIDER_LABELS}
        providers={data.providers}
        fallbackEnabled={data.fallback_enabled}
        saving={orderMutation.isPending || fallbackMutation.isPending}
        onReorder={(order) => orderMutation.mutate(order)}
        onToggleFallback={(enabled) => fallbackMutation.mutate(enabled)}
      />
      {priorityError && <p className="text-sm text-destructive">{priorityError}</p>}

      <Card>
        <CardHeader>
          <CardTitle>Monthly budget</CardTitle>
        </CardHeader>
        <CardContent className="space-y-2">
          <Label htmlFor="ai-budget">Monthly AI budget (USD)</Label>
          <Input
            id="ai-budget"
            type="number"
            inputMode="decimal"
            min={0.01}
            step={0.01}
            value={budgetInput}
            onChange={(e) => { setBudgetMessage(null); setBudgetInput(e.target.value); }}
            placeholder="No cap"
            data-testid="ai-budget-input"
            aria-invalid={budgetInvalid || undefined}
            aria-describedby={budgetInvalid ? "ai-budget-help ai-budget-error" : "ai-budget-help"}
          />
          {budgetInvalid && (
            <p id="ai-budget-error" className="text-xs text-destructive" data-testid="ai-budget-error">
              Enter an amount greater than $0, or leave blank for no cap.
            </p>
          )}
          <p className="text-sm text-muted-foreground" data-testid="ai-budget-status">
            {data.monthly_budget_usd != null
              ? `Current cap: ${formatUsd(data.monthly_budget_usd)} per month`
              : "No cap set."}
          </p>
          {usage?.budget && (
            <p className="text-sm text-muted-foreground" data-testid="ai-budget-spent">
              Spent this month: {formatUsd(usage.budget.spent_month_usd)} · resets{" "}
              {new Date(usage.budget.resets_at).toLocaleDateString("en-US", {
                month: "short",
                day: "numeric",
                timeZone: "UTC",
              })}
            </p>
          )}
          <p id="ai-budget-help" className="text-xs text-muted-foreground">
            Leave blank for no cap. When reached, AI features pause until the 1st of next month. Alerts
            still send without AI analysis.
          </p>
          <div className="flex items-center gap-3 pt-2">
            <Button
              type="button"
              onClick={() => budgetMutation.mutate()}
              disabled={!budgetChanged || budgetInvalid || budgetMutation.isPending}
            >
              {budgetMutation.isPending ? "Saving…" : "Save"}
            </Button>
            <SaveStatus message={budgetMessage} />
          </div>
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle className="flex items-center justify-between gap-2">
            <span>Data sent to AI</span>
            <label className="flex items-center gap-2 text-sm font-normal text-muted-foreground">
              Include sample values in AI prompts
              <Switch
                checked={data.include_sample_values}
                disabled={sampleValuesMutation.isPending}
                onCheckedChange={(enabled) => sampleValuesMutation.mutate(enabled)}
              />
            </label>
          </CardTitle>
        </CardHeader>
        <CardContent className="space-y-3">
          <p className="text-xs text-muted-foreground">
            Ask with AI, Auto Visualize, and Suggest checks see your schema, a few sample values per
            column, your SQL, and the error message. Your query results are sent only when Explain
            alerts or Explain dashboards below is on. Turn this off to stop sending sample values and
            results; column stats are still used.
          </p>
          <Separator />
          <label className="flex items-center justify-between gap-2 text-sm font-medium">
            Explain alerts with AI
            <Switch
              id="ai-explain-alerts"
              checked={(data.explain_alerts ?? false) && data.include_sample_values}
              disabled={explainMutation.isPending || !data.include_sample_values}
              onCheckedChange={(enabled) => explainMutation.mutate(enabled)}
              aria-describedby="ai-explain-alerts-help"
              data-testid="ai-explain-alerts-switch"
            />
          </label>
          {!data.include_sample_values && (
            <p className="text-xs text-muted-foreground">Requires &lsquo;Include sample values&rsquo;.</p>
          )}
          <p id="ai-explain-alerts-help" className="text-xs text-muted-foreground">
            When a monitor fires, AI runs up to 3 read-only aggregate queries on the monitor&apos;s data
            source and adds a short explanation to the alert.{" "}
            <span className="font-medium text-foreground">
              Those aggregated rows are sent to your AI provider.
            </span>{" "}
            Only data sources with an AI profile are used. When off, alerts still include a plain
            summary of what changed.
          </p>
          <Separator />
          <label className="flex items-center justify-between gap-2 text-sm font-medium">
            Explain dashboards with AI
            <Switch
              id="ai-dashboard-narratives"
              checked={(data.dashboard_narratives ?? true) && data.include_sample_values}
              disabled={narrativesMutation.isPending || !data.include_sample_values}
              onCheckedChange={(enabled) => narrativesMutation.mutate(enabled)}
              aria-describedby="ai-dashboard-narratives-help"
              data-testid="ai-dashboard-narratives-switch"
            />
          </label>
          {!data.include_sample_values && (
            <p className="text-xs text-muted-foreground">Requires &lsquo;Include sample values&rsquo;.</p>
          )}
          <p id="ai-dashboard-narratives-help" className="text-xs text-muted-foreground">
            Adds an Explain button to dashboards and an AI brief for favorite dashboards in the daily
            digest. Explain and the morning brief send up to 20 rows per widget to your AI provider.
            Briefs use each widget&apos;s latest results; queries are never re-run.
          </p>
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle>Usage limit</CardTitle>
        </CardHeader>
        <CardContent className="space-y-2">
          <Label htmlFor="rate-limit">AI feature calls per user, per minute</Label>
          <Input
            id="rate-limit"
            type="number"
            min={1}
            step={1}
            value={rateLimitInput}
            onChange={(e) => { setRateLimitMessage(null); setRateLimitInput(e.target.value); }}
            placeholder={`System default: ${data.rate_limit_env_default}`}
          />
          <p className="text-sm text-muted-foreground">
            {data.rate_limit_org_value != null
              ? `Organization override: ${data.rate_limit_org_value}/min`
              : `Using system default (${data.rate_limit_env_default}/min)`}
          </p>
          <p className="text-xs text-muted-foreground">
            Limits how often each user can call AI features — Ask with AI, Auto Visualize, and
            Suggest checks. Leave blank to use the system default (
            {data.rate_limit_env_default}/min). <DocsLink topic="aiUsageLimits" label="AI usage limits" />
          </p>
          <div className="flex items-center gap-3 pt-2">
            <Button
              type="button"
              onClick={() => rateLimitMutation.mutate()}
              disabled={!rateLimitChanged || rateLimitMutation.isPending}
            >
              {rateLimitMutation.isPending ? "Saving…" : "Save"}
            </Button>
            <SaveStatus message={rateLimitMessage} />
          </div>
        </CardContent>
      </Card>

      <ConfirmDialog
        open={removeDialogKind != null}
        onOpenChange={(open) => !open && closeRemoveDialog()}
        title={`Remove ${removeDialogKind ? PROVIDER_LABELS[removeDialogKind] : ""} key?`}
        body={
          <p className="text-sm text-muted-foreground">
            &quot;Ask with AI&quot; will use the system-configured key for this provider if one exists, or be
            disabled for it otherwise.
          </p>
        }
        error={removeError}
        destructive
        confirmLabel="Remove key"
        pendingLabel="Removing…"
        confirmPending={removeMutation.isPending}
        allowCancelWhilePending
        onConfirm={() => removeDialogKind && removeMutation.mutate(removeDialogKind)}
      />
    </div>
  );
}
