/** Opt-in live check of the Ask loop against a real provider. Run: ARCODASH_LIVE_AI=1
 * ANTHROPIC_API_KEY=... pnpm test:e2e tests/e2e/ai-ask-live.spec.ts (export only the provider key;
 * sourcing the full .env overrides the e2e admin credentials). Accepts
 * GEMINI_/OPENAI_/ANTHROPIC_API_KEY (first set wins); force one with ARCODASH_LIVE_AI_PROVIDER. */
import { test, expect, type APIRequestContext } from "@playwright/test";
import { adminContext, csrf } from "./helpers/auth";
import { loginAsAdmin, TARGET_DS_OPTIONS } from "./helpers/api";
import { Client } from "pg";
import { getUser, queryRows } from "./helpers/db";
import { delKey } from "./helpers/valkey";

test.skip(!process.env.ARCODASH_LIVE_AI, "live provider check: set ARCODASH_LIVE_AI=1 (and GEMINI_API_KEY, OPENAI_API_KEY or ANTHROPIC_API_KEY)");
test.use({ trace: "off", screenshot: "off" });
test.describe.configure({ mode: "serial", timeout: 300_000 });

const TARGET_DB_URL = "postgres://target:target@localhost:5434/target_db";
const SEED_EMAILS = ["live-ai-ann@example.test", "live-ai-bob@example.test", "live-ai-cy@example.test"];

/** The fixture tables ship empty; seed a few rows so the previews have something to show. */
async function seedTarget(): Promise<void> {
  const c = new Client({ connectionString: TARGET_DB_URL });
  await c.connect();
  try {
    await c.query("DELETE FROM orders WHERE customer_id IN (SELECT id FROM customers WHERE email = ANY($1))", [SEED_EMAILS]);
    await c.query("DELETE FROM customers WHERE email = ANY($1)", [SEED_EMAILS]);
    await c.query("INSERT INTO customers (email, phone) SELECT e, '555-0100' FROM unnest($1::text[]) e", [SEED_EMAILS]);
    await c.query(
      `INSERT INTO orders (customer_id, created_at)
       SELECT id, now() - interval '2 days' FROM customers WHERE email = $1
       UNION ALL SELECT id, now() - interval '90 days' FROM customers WHERE email = $2`,
      [SEED_EMAILS[0], SEED_EMAILS[1]],
    );
  } finally {
    await c.end();
  }
}

async function unseedTarget(): Promise<void> {
  const c = new Client({ connectionString: TARGET_DB_URL });
  await c.connect();
  try {
    await c.query("DELETE FROM orders WHERE customer_id IN (SELECT id FROM customers WHERE email = ANY($1))", [SEED_EMAILS]);
    await c.query("DELETE FROM customers WHERE email = ANY($1)", [SEED_EMAILS]);
  } finally {
    await c.end();
  }
}

const Q1 = "customers with no orders in the last 30 days";
const Q2 = "number of orders per customer, highest first";
const READ_ONLY = /^\s*(select|with)\b/i;
const WRITE_KW = /\b(insert|update|delete|drop|alter|truncate|create|grant)\b/i;

type SseEvent = { event: string; data: Record<string, unknown> };

function parseSse(body: string): SseEvent[] {
  return body
    .split("\n\n")
    .map((block) => {
      const event = /^event: (.*)$/m.exec(block)?.[1];
      const data = /^data: (.*)$/m.exec(block)?.[1];
      return event && data ? { event, data: JSON.parse(data) } : null;
    })
    .filter((e): e is SseEvent => e !== null);
}

async function ask(req: APIRequestContext, headers: Record<string, string>, dsId: number, question: string) {
  const res = await req.post("/api/queries/ask", {
    headers,
    data: { data_source_id: dsId, question },
    timeout: 120_000,
  });
  expect(res.status()).toBe(200);
  return parseSse(await res.text());
}

function report(label: string, events: SseEvent[]) {
  const final = events.find((e) => e.event === "final")?.data ?? {};
  console.log(
    `[live-ai] ${label} events=${events.map((e) => e.event).join(" → ")}\n` +
      `[live-ai] ${label} sql=${final.sql} attempts=${final.attempts} tokens_in=${final.tokens_in} ` +
      `tokens_out=${final.tokens_out} served_by=${final.served_by}`,
  );
}

function expectGoodSequence(events: SseEvent[]) {
  const names = events.map((e) => e.event).filter((n) => n !== "repair");
  expect(names[0]).toBe("status");
  expect(names).toContain("draft");
  expect(names).toContain("executing");
  expect(names).toContain("preview");
  expect(names.at(-1)).toBe("final");
}

test("live: profile → key → Ask (UI + SSE) → descriptions → second ask", async ({ browser }) => {
  const keyVars = { gemini: "GEMINI_API_KEY", openai: "OPENAI_API_KEY", anthropic: "ANTHROPIC_API_KEY" } as const;
  const forced = process.env.ARCODASH_LIVE_AI_PROVIDER as keyof typeof keyVars | undefined;
  if (forced && !(forced in keyVars)) throw new Error("ARCODASH_LIVE_AI_PROVIDER must be gemini, openai or anthropic");
  const provider = forced ?? (Object.keys(keyVars) as (keyof typeof keyVars)[]).find((p) => process.env[keyVars[p]]) ?? "gemini";
  const key = process.env[keyVars[provider]];
  test.skip(!key, `${keyVars[provider]} is not set (needed for provider ${provider})`);

  const startedAt = new Date(Date.now() - 60_000).toISOString(); // slack for clock skew
  const api = await loginAsAdmin();
  const dsName = `e2e-ask-live-${Date.now()}`;
  const ctx = await adminContext(browser);
  const headers = await csrf(ctx);
  const req = ctx.request;
  type AiSettings = {
    include_sample_values: boolean;
    fallback_enabled: boolean;
    provider_order: string[];
    providers: Record<string, { key_source: string }>;
  };
  const getAi = async (): Promise<AiSettings> => (await req.get("/api/organization/ai")).json();
  const original = await getAi();
  if (original.providers[provider]?.key_source === "org") {
    await ctx.close();
    test.skip(true, `org already has its own ${provider} key; refusing to overwrite or delete it`);
  }
  let created: { id: number } | null = null;
  try {
    await seedTarget();
    const ds = (created = await api.createDataSource({
      name: dsName,
      type: "pg",
      options: TARGET_DS_OPTIONS,
    }));
    // Data-source ids get reused; a prior run's describe guard/back-off would otherwise suppress
    // the job.
    await delKey(`arcodash:ai_profile:describing:${ds.id}`);
    const schemaRes = await req.get(`/api/data_sources/${ds.id}/schema?refresh=true`);
    expect(schemaRes.ok()).toBeTruthy();
    const rebuild = await req.post(`/api/data_sources/${ds.id}/ai_profile/rebuild`, { headers });
    expect([202, 409]).toContain(rebuild.status());

    type Profile = {
      tables: { name: string; columns: { name: string; samples: string[]; description?: string | null }[] }[];
      relationships: unknown[];
    };
    const getProfile = async (): Promise<Profile | null> =>
      (await (await req.get(`/api/data_sources/${ds.id}/ai_profile`)).json()).profile;

    await expect.poll(getProfile, { timeout: 60_000, intervals: [1000] }).not.toBeNull();
    const profile = (await getProfile())!;
    const names = profile.tables.map((t) => t.name);
    console.log(`[live-ai] profile tables=${names.join(",")} relationships=${profile.relationships.length}`);
    expect(profile.tables.flatMap((t) => t.columns).some((c) => c.samples.length > 0)).toBe(true);
    expect(names).toEqual(expect.arrayContaining(["customers", "orders"]));
    expect(profile.relationships.length).toBeGreaterThan(0);

    const saved = await req.post("/api/organization/ai", {
      headers,
      data: {
        providers: { [provider]: { api_key: key } },
        provider_order: [provider],
        fallback_enabled: false,
        include_sample_values: true,
      },
    });
    expect(saved.ok()).toBeTruthy();

    const page = await ctx.newPage();
    await page.goto("/queries/new");
    const picker = page.getByTestId("query-editor-data-source-select");
    await picker.click();
    await page.getByRole("option", { name: dsName }).click();
    await expect(picker).toContainText(dsName);
    await page.getByRole("button", { name: "✨ Ask with AI" }).click();
    await page.getByLabel("Ask AI a question about your data").fill(Q1);
    const askReq = page.waitForRequest((r) => r.url().endsWith("/api/queries/ask"));
    await page.getByRole("button", { name: "Generate SQL" }).click();
    expect((await askReq).postDataJSON().data_source_id).toBe(ds.id);

    const timeline = page.getByTestId("ask-timeline");
    await expect(timeline).toHaveAttribute("data-phase", "done", { timeout: 120_000 });
    // Same request again, raw, to capture the event sequence (logged before asserting).
    const events = await ask(req, headers, ds.id, Q1);
    report("ask#1", events);
    await expect(page.getByRole("button", { name: /^Solved in/ })).toBeVisible();
    await expect(page.getByTestId("ask-preview-grid")).toBeVisible();
    const uiSql = (await page.locator(".cm-content").first().innerText()).trim();
    console.log(`[live-ai] UI sql=${uiSql}`);
    expect(uiSql).toMatch(READ_ONLY);
    expect(uiSql).not.toMatch(WRITE_KW);

    expectGoodSequence(events);
    const final = events.find((e) => e.event === "final")!.data;
    expect(String(final.sql)).toMatch(READ_ONLY);
    expect(String(final.sql)).not.toMatch(WRITE_KW);
    expect(final.executed).toBe(true);

    await expect
      .poll(async () => (await getProfile())?.tables.some((t) => t.columns.some((c) => c.description)), {
        timeout: 60_000,
        intervals: [2000],
      })
      .toBe(true);
    const described = (await getProfile())!.tables.flatMap((t) => t.columns).filter((c) => c.description);
    console.log(`[live-ai] described columns=${described.length} e.g. ${described[0].name}: ${described[0].description}`);

    const orgId = (await getUser(process.env.ADMIN_EMAIL ?? "admin@example.com")).org_id;
    await expect
      .poll(
        async () =>
          Number(
            (
              await queryRows<{ n: string }>(
                "SELECT count(*) AS n FROM ai_usage WHERE feature = 'describe_schema' AND org_id = $1 AND created_at >= $2::timestamptz",
                [orgId, startedAt],
              )
            )[0].n,
          ),
        { timeout: 30_000, intervals: [1000] },
      )
      .toBeGreaterThanOrEqual(1);

    const events2 = await ask(req, headers, ds.id, Q2);
    report("ask#2", events2);
    expect(events2.at(-1)?.event).toBe("final");
  } finally {
    // Never leave a real provider key (or the sample-values opt-in) in the org.
    {
      const restore = await req
        .post("/api/organization/ai", {
          headers,
          data: {
            providers: { [provider]: { api_key: null } },
            provider_order: original.provider_order,
            fallback_enabled: original.fallback_enabled,
            include_sample_values: original.include_sample_values,
          },
        })
        .catch(() => null);
      if (!restore?.ok()) console.log("[live-ai] cleanup: restoring AI settings failed");
      const after = await getAi().catch(() => null);
      expect.soft(after?.providers[provider].key_source).not.toBe("org");
      expect.soft(after?.include_sample_values).toBe(original.include_sample_values);
      expect.soft(after?.fallback_enabled).toBe(original.fallback_enabled);
      console.log(
        `[live-ai] settings restored: key_source=${after?.providers[provider].key_source} ` +
          `include_sample_values=${after?.include_sample_values} fallback_enabled=${after?.fallback_enabled}`,
      );
    }
    if (created) await api.deleteDataSource(created.id).catch(() => {});
    await unseedTarget().catch(() => {});
    await ctx.close();
  }
});
