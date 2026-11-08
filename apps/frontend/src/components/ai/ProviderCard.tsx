import { useEffect, useState } from "react";
import { ExternalLink } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { ProviderStatusBadge, STATUS_CLASS } from "@/components/ai/ProviderStatusBadge";
import { exactTs, timeAgo } from "@/lib/scheduling";
import type { AiProviderInfo, TestKeyResult } from "@/types/api";

const KEY_FORMAT = /^\S{20,}$/;
const CUSTOM_MODEL = "__custom__";
const FREE_TIER_NOTICE_MS = 48 * 3600 * 1000;

export interface ProviderSaveBody {
  api_key?: string | null;
  model?: string | null;
  base_url?: string | null;
}

interface ProviderCardProps {
  kind: string;
  label: string;
  info: AiProviderInfo;
  keyHelp: string;
  saving: boolean;
  saveMessage?: { kind: "success" | "error"; text: string } | null;
  onSave: (kind: string, body: ProviderSaveBody) => void;
  testing: boolean;
  testResult: TestKeyResult | null;
  onTest: (kind: string, apiKey?: string) => void;
  onRequestRemove: (kind: string) => void;
}

/** One provider's key + model (+ base URL for OpenAI) form, self-contained and parameterized by
 * `kind`. */
export function ProviderCard({
  kind,
  label,
  info,
  keyHelp,
  saving,
  saveMessage,
  onSave,
  testing,
  testResult,
  onTest,
  onRequestRemove,
}: ProviderCardProps) {
  const [keyInput, setKeyInput] = useState("");
  const [modelChoice, setModelChoice] = useState<string>(info.model ?? info.default_model);
  const [customModel, setCustomModel] = useState("");
  const [baseUrlInput, setBaseUrlInput] = useState(info.base_url ?? "");

  // Re-sync on server data load; keyed on key_configured too, so removing a key still clears a
  // stale draft key.
  useEffect(() => {
    const isCurated = info.model != null && info.curated_models.includes(info.model);
    setModelChoice(isCurated ? (info.model as string) : CUSTOM_MODEL);
    setCustomModel(isCurated ? "" : info.model ?? "");
    setBaseUrlInput(info.base_url ?? "");
    setKeyInput("");
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [info.model, info.base_url, info.key_configured, info.key_source]);

  const trimmedKey = keyInput.trim();
  const keyFormatError = trimmedKey !== "" && !KEY_FORMAT.test(trimmedKey);
  const canTestKey = trimmedKey !== "" ? !keyFormatError : info.key_configured;

  const effectiveModel = modelChoice === CUSTOM_MODEL ? customModel.trim() : modelChoice;
  const currentModel = info.model ?? info.default_model;
  const modelChanged = effectiveModel !== "" && effectiveModel !== currentModel;
  const trimmedBaseUrl = baseUrlInput.trim();
  const baseUrlChanged = kind === "openai" && trimmedBaseUrl !== (info.base_url ?? "");

  const canSave = !keyFormatError && (trimmedKey !== "" || modelChanged || baseUrlChanged);
  const saveBlockedReason = keyFormatError
    ? "Fix the API key above"
    : "Enter an API key or change the model/base URL first";

  const freeTierMs = info.free_tier_seen_at ? Date.parse(info.free_tier_seen_at) : NaN;
  const now = Date.now();
  const showFreeTier = Number.isFinite(freeTierMs) && now - freeTierMs <= FREE_TIER_NOTICE_MS;

  return (
    <Card data-testid={`provider-card-${kind}`}>
      <CardHeader>
        <CardTitle className="flex items-center justify-between gap-2">
          <span>{label}</span>
          <ProviderStatusBadge status={info.last_status} />
        </CardTitle>
      </CardHeader>
      <CardContent className="space-y-4">
        {info.key_configured ? (
          <p className="text-sm text-success">
            {info.key_source === "org"
              ? <>✓ Using your organization&apos;s key <span className="font-mono text-muted-foreground">••••••••</span></>
              : "✓ Using system-configured key (set via environment). Adding a key here will override it."}
          </p>
        ) : (
          <p className="text-sm text-destructive">⚠ No API key configured. {keyHelp}</p>
        )}

        {showFreeTier && (
          <div
            role="status"
            data-testid={`provider-free-tier-${kind}`}
            className="rounded-md border border-warning/30 bg-warning/10 px-3 py-2 text-sm text-warning-foreground space-y-1"
          >
            <p className="font-medium">This key is on Google&apos;s free tier</p>
            <p>
              Google allows free-tier keys only a few requests per model per day, and drops them first
              when it&apos;s busy. That&apos;s why AI requests sometimes fail with &quot;quota
              exhausted&quot; or &quot;busy&quot;. To fix it, enable billing on the Google Cloud project
              this key belongs to. The same key then gets paid limits, so you don&apos;t need to change
              anything here.
            </p>
            <a
              href="https://ai.google.dev/gemini-api/docs/rate-limits"
              target="_blank"
              rel="noopener noreferrer"
              className="inline-flex items-center gap-1 underline underline-offset-2"
            >
              Google&apos;s rate limits and billing
              <ExternalLink className="size-3" aria-hidden="true" />
            </a>
            <p className="text-xs opacity-80" title={exactTs(freeTierMs)}>
              Last seen {timeAgo(freeTierMs, now)}.
            </p>
          </div>
        )}

        <div className="space-y-2">
          <Label htmlFor={`${kind}-key`}>API key</Label>
          <Input
            id={`${kind}-key`}
            type="password"
            autoComplete="new-password"
            value={keyInput}
            onChange={(e) => setKeyInput(e.target.value)}
            placeholder={info.key_source === "org" ? "•••••••• — leave blank to keep current" : undefined}
            aria-invalid={keyFormatError}
          />
          {keyFormatError && (
            <p className="text-sm text-destructive">
              That doesn&apos;t look like a complete API key — check for missing characters or extra spaces.
            </p>
          )}
        </div>

        <div className="space-y-2">
          <Label htmlFor={`${kind}-model`}>Model</Label>
          <Select value={modelChoice} onValueChange={setModelChoice}>
            <SelectTrigger id={`${kind}-model`} className="w-full">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              {info.curated_models.map((m) => (
                <SelectItem key={m} value={m}>
                  {m}
                </SelectItem>
              ))}
              <SelectItem value={CUSTOM_MODEL}>Custom…</SelectItem>
            </SelectContent>
          </Select>
          {modelChoice === CUSTOM_MODEL && (
            <Input
              value={customModel}
              onChange={(e) => setCustomModel(e.target.value)}
              placeholder={`e.g. ${info.default_model}`}
            />
          )}
        </div>

        {kind === "openai" && (
          <div className="space-y-2">
            <Label htmlFor="openai-base-url">Base URL</Label>
            <Input
              id="openai-base-url"
              value={baseUrlInput}
              onChange={(e) => setBaseUrlInput(e.target.value)}
              placeholder="https://api.openai.com"
            />
            <p className="text-xs text-muted-foreground">
              Works with any OpenAI-compatible API — Mistral, xAI, OpenRouter…
            </p>
          </div>
        )}

        <div className="flex items-center gap-3">
          <Button
            type="button"
            variant="outline"
            onClick={() => onTest(kind, trimmedKey || undefined)}
            disabled={!canTestKey || testing}
          >
            {testing ? "Testing…" : "Test key"}
          </Button>
          <Button
            type="button"
            onClick={() =>
              onSave(kind, {
                ...(trimmedKey !== "" ? { api_key: trimmedKey } : {}),
                ...(modelChanged ? { model: effectiveModel } : {}),
                ...(baseUrlChanged ? { base_url: trimmedBaseUrl === "" ? null : trimmedBaseUrl } : {}),
              })
            }
            disabled={!canSave || saving}
            title={!canSave ? saveBlockedReason : undefined}
          >
            {saving ? "Saving…" : "Save"}
          </Button>
          {saveMessage && (
            <span className={`text-sm ${saveMessage.kind === "success" ? "text-success" : "text-destructive"}`}>
              {saveMessage.text}
            </span>
          )}
          {info.key_source === "org" && (
            <Button
              type="button"
              variant="ghost"
              size="sm"
              className="ml-auto text-destructive hover:bg-destructive/10 hover:text-destructive"
              onClick={() => onRequestRemove(kind)}
            >
              Remove key
            </Button>
          )}
        </div>

        {testResult && (
          <p className={`rounded-md border px-3 py-2 text-sm ${STATUS_CLASS[testResult.status]}`}>
            {testResult.message}
          </p>
        )}
      </CardContent>
    </Card>
  );
}
