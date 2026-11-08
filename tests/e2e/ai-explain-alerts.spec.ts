// Live run (opt-in): ARCODASH_LIVE_AI=1 ARCODASH_LIVE_AI_PROVIDER=anthropic ANTHROPIC_API_KEY=...
// pnpm test:e2e tests/e2e/ai-explain-alerts.spec.ts
import { test, expect, type BrowserContext } from "@playwright/test";
import { Client } from "pg";
import { acquireOrgAiLock } from "./helpers/org-ai-lock";
import { adminContext, csrf } from "./helpers/auth";
import { loginAsAdmin, TARGET_DS_OPTIONS } from "./helpers/api";
import { queryRows } from "./helpers/db";
import { delKey, enqueueEvaluateMonitor } from "./helpers/valkey";
import { MAILPIT_URL } from "./helpers/mailpit";

test.use({ trace: "off", screenshot: "off" });

const TARGET_DB_URL = "postgres://target:target@localhost:5434/target_db";

type AiSettings = {
  include_sample_values: boolean;
  explain_alerts: boolean;
  fallback_enabled: boolean;
  provider_order: string[];
  providers: Record<string, { key_source: string }>;
};

test.describe("explain alerts settings", () => {
  test("switch toggles, persists across reload, and is gated by sample values", async ({ browser }) => {
    // Headroom covers waiting on the org AI lock.
    test.setTimeout(90_000);
    const releaseOrgAi = await acquireOrgAiLock();
    let ctx: BrowserContext | undefined;
    let restore: (() => Promise<unknown>) | undefined;
    try {
      ctx = await adminContext(browser);
      const req = ctx.request;
      const headers = await csrf(ctx);
      const getAi = async (): Promise<AiSettings> => (await req.get("/api/organization/ai")).json();
      const setAi = (data: Record<string, unknown>) => req.post("/api/organization/ai", { headers, data });
      const original = await getAi();
      restore = () =>
        setAi({ include_sample_values: original.include_sample_values, explain_alerts: original.explain_alerts });
      expect((await setAi({ include_sample_values: true, explain_alerts: false })).ok()).toBeTruthy();
      const page = await ctx.newPage();
      await page.goto("/organization/ai");
      const sw = page.getByTestId("ai-explain-alerts-switch");
      await expect(sw).toBeEnabled();
      await expect(sw).not.toBeChecked();
      await expect(page.getByText("Those aggregated rows are sent to your AI provider.")).toBeVisible();

      await sw.click();
      await expect(sw).toBeChecked();
      await expect.poll(async () => (await getAi()).explain_alerts).toBe(true);
      await page.reload();
      await expect(page.getByTestId("ai-explain-alerts-switch")).toBeChecked();

      await page.getByTestId("ai-explain-alerts-switch").click();
      await expect(page.getByTestId("ai-explain-alerts-switch")).not.toBeChecked();
      await expect.poll(async () => (await getAi()).explain_alerts).toBe(false);
      await page.reload();
      await expect(page.getByTestId("ai-explain-alerts-switch")).not.toBeChecked();

      expect((await setAi({ include_sample_values: false })).ok()).toBeTruthy();
      await page.reload();
      await expect(page.getByTestId("ai-explain-alerts-switch")).toBeDisabled();
      await expect(page.getByTestId("ai-dashboard-narratives-switch")).toBeDisabled();
      await expect(page.getByText("Requires ‘Include sample values’.")).toHaveCount(2);
    } finally {
      await restore?.().catch(() => {});
      await ctx?.close();
      releaseOrgAi();
    }
  });
});

test.describe("explain alerts live", () => {
  test.skip(!process.env.ARCODASH_LIVE_AI, "live provider check: set ARCODASH_LIVE_AI=1 and a provider key");
  test("threshold alert carries an AI analysis in the email and the incident card", async ({ browser }, testInfo) => {
    testInfo.setTimeout(420_000);
    const keyVars = { gemini: "GEMINI_API_KEY", openai: "OPENAI_API_KEY", anthropic: "ANTHROPIC_API_KEY" } as const;
    const forced = process.env.ARCODASH_LIVE_AI_PROVIDER as keyof typeof keyVars | undefined;
    if (forced && !(forced in keyVars)) throw new Error("ARCODASH_LIVE_AI_PROVIDER must be gemini, openai or anthropic");
    const provider =
      forced ?? (Object.keys(keyVars) as (keyof typeof keyVars)[]).find((p) => process.env[keyVars[p]]) ?? "gemini";
    const key = process.env[keyVars[provider]];
    test.skip(!key, `${keyVars[provider]} is not set (needed for provider ${provider})`);

    const api = await loginAsAdmin();
    const ctx = await adminContext(browser);
    const headers = await csrf(ctx);
    const req = ctx.request;
    const getAi = async (): Promise<AiSettings> => (await req.get("/api/organization/ai")).json();
    const original = await getAi();
    if (original.providers[provider]?.key_source === "org") {
      await ctx.close();
      test.skip(true, `org already has its own ${provider} key; refusing to overwrite or delete it`);
    }

    const stamp = Date.now();
    const table = `e2e_explain_${stamp}`;
    const monitorName = `e2e-explain-monitor-${stamp}`;
    const pg = new Client({ connectionString: TARGET_DB_URL });
    await pg.connect();
    let dsId: number | undefined;
    let queryId: number | undefined;
    let monitorId: number | undefined;
    let destId: number | undefined;
    try {
      await pg.query(`CREATE TABLE ${table} (id serial PRIMARY KEY, segment text NOT NULL, amount int NOT NULL)`);
      await pg.query(
        `INSERT INTO ${table} (segment, amount) SELECT s, 1 FROM unnest(ARRAY['alpha','beta','gamma']) s, generate_series(1, 4)`,
      );

      const ds = await api.createDataSource({
        name: `e2e-explain-ds-${stamp}`,
        type: "pg",
        options: TARGET_DS_OPTIONS,
      });
      dsId = ds.id;
      await delKey(`arcodash:ai_profile:describing:${ds.id}`);
      expect((await req.get(`/api/data_sources/${ds.id}/schema?refresh=true`)).ok()).toBeTruthy();
      const rebuild = await req.post(`/api/data_sources/${ds.id}/ai_profile/rebuild`, { headers });
      expect([202, 409]).toContain(rebuild.status());
      const profileHasTable = async () => {
        const p = (await (await req.get(`/api/data_sources/${ds.id}/ai_profile`)).json()).profile as {
          tables: { name: string }[];
        } | null;
        return p?.tables.some((t) => t.name === table) ?? false;
      };
      await expect.poll(profileHasTable, { timeout: 90_000, intervals: [1000] }).toBe(true);

      const saved = await req.post("/api/organization/ai", {
        headers,
        data: {
          providers: { [provider]: { api_key: key } },
          provider_order: [provider],
          fallback_enabled: false,
          include_sample_values: true,
          explain_alerts: true,
        },
      });
      expect(saved.ok()).toBeTruthy();

      const q = await api.createQuery({
        data_source_id: ds.id,
        name: `e2e-explain-q-${stamp}`,
        query: `SELECT count(*) AS orders FROM ${table}`,
      });
      queryId = q.id;
      await api.publishQuery(q.id);
      const dest = await api.createDestination({
        name: `e2e-explain-dest-${stamp}`,
        type: "email",
        options: { to: `${monitorName}@example.com` },
      });
      destId = dest.id;
      const monitor = await api.createMonitor({
        query_id: q.id,
        name: monitorName,
        options: { column: "orders", op: ">", value: 20 },
      });
      monitorId = monitor.id;
      await api.createMonitorSubscription(monitor.id, { destination_id: dest.id });
      const orgId = Number(
        (await queryRows<{ org_id: number }>("SELECT org_id FROM monitors WHERE id = $1", [monitor.id]))[0].org_id,
      );

      const executeAndWait = async () => {
        const res = await api.executeQuery(q.id);
        if (!("job" in res) || !res.job) return;
        const client = api as unknown as { fetch<T>(p: string): Promise<T> };
        for (let i = 0; i < 60; i++) {
          const { job } = await client.fetch<{ job: { status: number; error: string | null } }>(`/api/jobs/${res.job.id}`);
          if (job.status === 3) return;
          if (job.status === 4 || job.status === 5) throw new Error(`Job ended: ${job.error}`);
          await new Promise((r) => setTimeout(r, 500));
        }
        throw new Error("Job did not complete");
      };

      await executeAndWait();
      await enqueueEvaluateMonitor(orgId, monitor.id);
      await expect
        .poll(async () => (await queryRows<{ n: string }>("SELECT count(*) AS n FROM monitor_observations WHERE monitor_id = $1", [monitor.id]))[0].n, {
          timeout: 60_000,
          intervals: [1000],
        })
        .toBe("1");
      await pg.query(`INSERT INTO ${table} (segment, amount) SELECT 'beta', 1 FROM generate_series(1, 40)`);
      // Identical SQL would be served from the result cache; a published edit forces a fresh run.
      await (api as unknown as { fetch(p: string, i?: RequestInit): Promise<unknown> }).fetch(`/api/queries/${q.id}`, {
        method: "POST",
        body: JSON.stringify({ query: `SELECT count(*) AS orders FROM ${table} /* rerun */` }),
      });
      await api.publishQuery(q.id);
      await executeAndWait();
      await enqueueEvaluateMonitor(orgId, monitor.id);

      await expect
        .poll(async () => (await queryRows<{ state: string }>("SELECT state FROM monitors WHERE id = $1", [monitor.id]))[0]?.state, {
          timeout: 60_000,
          intervals: [1000],
        })
        .toBe("triggered");

      let body = "";
      await expect
        .poll(
          async () => {
            const list = (await (await fetch(`${MAILPIT_URL}/api/v1/messages?limit=50`)).json()) as {
              messages: { ID: string; Subject: string }[];
            };
            const m = list.messages?.find((x) => x.Subject?.includes(monitorName));
            if (!m) return "";
            const full = (await (await fetch(`${MAILPIT_URL}/api/v1/message/${m.ID}`)).json()) as {
              Text?: string;
              HTML?: string;
            };
            body = `${full.Text ?? ""}\n${full.HTML ?? ""}`;
            return body;
          },
          { timeout: 180_000, intervals: [3000] },
        )
        .toContain("AI analysis (confidence:");
      console.log(`[live-ai] email narrative: ${/AI analysis \(confidence:[^\n<]*/.exec(body)?.[0]}`);

      const page = await ctx.newPage();
      await page.goto(`/monitors/${monitor.id}`);
      const card = page.getByTestId("latest-incident");
      await expect(card).toBeVisible({ timeout: 30_000 });
      await expect(card.getByTestId("latest-incident-ai")).toBeVisible();
    } finally {
      const restore = await req
        .post("/api/organization/ai", {
          headers,
          data: {
            providers: { [provider]: { api_key: null } },
            provider_order: original.provider_order,
            fallback_enabled: original.fallback_enabled,
            include_sample_values: original.include_sample_values,
            explain_alerts: original.explain_alerts,
          },
        })
        .catch(() => null);
      if (!restore?.ok()) console.log("[live-ai] cleanup: restoring AI settings failed");
      const after = await getAi().catch(() => null);
      expect.soft(after?.providers[provider].key_source).not.toBe("org");
      expect.soft(after?.explain_alerts).toBe(original.explain_alerts);
      if (monitorId) await api.deleteMonitor(monitorId).catch(() => {});
      if (destId) await api.deleteDestination(destId).catch(() => {});
      if (queryId) await api.deleteQuery(queryId).catch(() => {});
      if (dsId) await api.deleteDataSource(dsId).catch(() => {});
      await pg.query(`DROP TABLE IF EXISTS ${table}`).catch(() => {});
      await pg.end().catch(() => {});
      await ctx.close();
    }
  });
});
