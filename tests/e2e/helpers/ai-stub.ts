/** Local OpenAI-compatible chat-completions stub for AI e2e specs (reachable because
 * playwright.config.ts sets ARCODASH_ALLOW_PRIVATE_CONNECTORS=1). */
import http from "node:http";
import type { AddressInfo } from "node:net";
import type { E2EApiClient } from "./api";
import { acquireOrgAiLock } from "./org-ai-lock";

export const STUB_SUMMARY = "Orders total is 4242 and holding steady.";

export interface Stub {
  baseUrl: string;
  prompts: string[];
  widgetId: number;
  widgetTitle: string;
  close: () => Promise<void>;
}

/** Records request bodies; answers with a canned narrative linking to `stub.widgetId`. */
export async function startStub(): Promise<Stub> {
  const prompts: string[] = [];
  const stub = { prompts, widgetId: 0, widgetTitle: "Widget" } as Stub;
  const server = http.createServer((req, res) => {
    let body = "";
    req.on("data", (c) => (body += c));
    req.on("end", () => {
      // Schema-description calls come from the data source's background AI profile build, at a
      // time the test doesn't control; only the prompts a test triggers are recorded.
      if (!body.includes("Describe each column of this database schema")) prompts.push(body);
      const content = JSON.stringify({
        summary: STUB_SUMMARY,
        body: `Orders total is **4242**. See [${stub.widgetTitle}](#widget-${stub.widgetId}) for detail.`,
        highlights: [],
        confidence: "high",
      });
      res.setHeader("content-type", "application/json");
      res.end(
        JSON.stringify({
          choices: [{ message: { content } }],
          usage: { prompt_tokens: 10, completion_tokens: 5 },
        }),
      );
    });
  });
  await new Promise<void>((r) => server.listen(0, "127.0.0.1", r));
  stub.baseUrl = `http://127.0.0.1:${(server.address() as AddressInfo).port}`;
  stub.close = () => new Promise((r) => server.close(() => r()));
  return stub;
}

type AiSettings = {
  include_sample_values: boolean;
  dashboard_narratives: boolean;
  provider_order: string[];
  providers: Record<string, { key_source: string }>;
};

/** Point the org's openai provider at the stub and enable narratives + sample values; returns a
 * restore function. */
export async function useStubProvider(api: E2EApiClient, stubUrl: string): Promise<() => Promise<void>> {
  const release = await acquireOrgAiLock();
  const set = (body: Record<string, unknown>) =>
    api.rawFetch("/api/organization/ai", { method: "POST", body: JSON.stringify(body) });
  let orig: AiSettings;
  try {
    orig = await api.rawFetch<AiSettings>("/api/organization/ai");
    await set({
      providers: { openai: { api_key: "sk-e2e-stub-key-0000000", base_url: stubUrl } },
      provider_order: ["openai"],
      include_sample_values: true,
      dashboard_narratives: true,
    });
  } catch (e) {
    release();
    throw e;
  }
  return async () => {
    const hadKey = orig.providers.openai?.key_source === "org";
    try {
      await set({
        ...(hadKey ? {} : { providers: { openai: { api_key: null, base_url: null } } }),
        provider_order: orig.provider_order,
        include_sample_values: orig.include_sample_values,
        dashboard_narratives: orig.dashboard_narratives,
      }).catch(() => {});
    } finally {
      release();
    }
  };
}

export function sse(events: [string, Record<string, unknown>][]): string {
  return events.map(([name, data]) => `event: ${name}\ndata: ${JSON.stringify(data)}\n\n`).join("");
}
