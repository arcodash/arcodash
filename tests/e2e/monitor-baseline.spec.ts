/** E2E: baseline (anomaly) monitor via the UI — steady evaluations stay ok with a "Learning" badge;
 * a 5x value flips it to triggered, the email carries "What changed", and the page shows "Latest
 * incident". Evaluations are forced via the raw Valkey job (run_id None path, still records
 * monitor_observations); values change by republishing the saved query. Requires backend :8080,
 * worker, target postgres :5434, mailpit :8026. */
import { test, expect } from "@playwright/test";
import { adminContext } from "./helpers/auth";
import { loginAsAdmin, E2EApiClient, TARGET_DS_OPTIONS } from "./helpers/api";
import { queryRows } from "./helpers/db";
import { enqueueEvaluateMonitor } from "./helpers/valkey";
import { MAILPIT_URL } from "./helpers/mailpit";

const STEADY = 42;
const SPIKE = STEADY * 5;
const NEED = 7;

async function executeAndWait(api: E2EApiClient, queryId: number): Promise<void> {
  const res = await api.executeQuery(queryId);
  if (!("job" in res) || !res.job) return;
  for (let i = 0; i < 60; i++) {
    const { job } = await api.rawFetch<{ job: { status: number; error: string | null } }>(
      `/api/jobs/${res.job.id}`,
    );
    if (job.status === 3) return;
    if (job.status === 4 || job.status === 5) throw new Error(`Job ended: ${job.error}`);
    await new Promise((r) => setTimeout(r, 500));
  }
  throw new Error("Job did not complete within timeout");
}

async function observationCount(monitorId: number): Promise<number> {
  const r = await queryRows<{ n: string }>(
    "SELECT count(*) AS n FROM monitor_observations WHERE monitor_id = $1",
    [monitorId],
  );
  return Number(r[0].n);
}

async function monitorState(monitorId: number): Promise<string | undefined> {
  const r = await queryRows<{ state: string }>("SELECT state FROM monitors WHERE id = $1", [monitorId]);
  return r[0]?.state;
}

async function evaluateAndWait(orgId: number, monitorId: number, expectedCount: number): Promise<void> {
  await enqueueEvaluateMonitor(orgId, monitorId);
  await expect
    .poll(() => observationCount(monitorId), { timeout: 60_000, intervals: [1000] })
    .toBe(expectedCount);
}

test("baseline monitor: learns 7 checks, then a 5x spike triggers with stats in email and incident card", async ({
  browser,
}, testInfo) => {
  testInfo.setTimeout(300_000);

  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();
  const monitorName = `e2e-baseline-${Date.now()}`;

  const ds = await api.createDataSource({
    name: `e2e-baseline-ds-${Date.now()}`,
    type: "pg",
    options: TARGET_DS_OPTIONS,
  });
  const q = await api.createQuery({
    data_source_id: ds.id,
    name: `e2e-baseline-q-${Date.now()}`,
    query: `SELECT int_col AS count FROM typed_table WHERE id = 1`, // int_col = 42 in the seed
  });
  await api.publishQuery(q.id);
  let monitorId: number | undefined;

  try {
    await executeAndWait(api, q.id);

    await page.goto("/monitors/new");
    await page.fill('input[placeholder="Monitor name"]', monitorName);
    await page.getByTestId("monitor-query-select").click();
    await page.getByRole("option", { name: q.name }).click();
    await page.getByTestId("monitor-column-select").click();
    await page.getByRole("option", { name: "count" }).click();
    await page.getByRole("button", { name: "Baseline" }).click();
    await expect(page.getByTestId("monitor-sensitivity-select")).toBeVisible();
    await page.click('button:has-text("Save")');
    await page.waitForURL(/\/monitors\/\d+$/, { timeout: 10_000 });
    monitorId = Number(page.url().match(/\/monitors\/(\d+)$/)![1]);

    const rows = await queryRows<{ org_id: number; options: { kind?: string; op?: unknown; value?: unknown } }>(
      "SELECT org_id, options FROM monitors WHERE id = $1",
      [monitorId],
    );
    const orgId = Number(rows[0].org_id);
    expect(rows[0].options.kind).toBe("baseline");
    expect(rows[0].options.op ?? null).toBeNull();
    expect(rows[0].options.value ?? null).toBeNull();

    const badge = page.getByTestId("monitor-learning-badge");
    for (let n = 1; n <= NEED; n++) {
      await evaluateAndWait(orgId, monitorId, n);
      expect(await monitorState(monitorId)).toBe("ok");
      await page.reload();
      if (n < NEED) {
        await expect(badge).toContainText("Learning");
        if (n === 1) {
          await badge.click();
          await expect(page.getByText("Learning the usual range")).toBeVisible();
          await expect(page.getByText(`${n} of ${NEED} checks`).first()).toBeVisible();
          await page.keyboard.press("Escape");
        }
      } else {
        await expect(badge).toHaveCount(0);
      }
    }

    // Only a published edit changes what the next execution returns.
    await api.rawFetch(`/api/queries/${q.id}`, {
      method: "POST",
      body: JSON.stringify({ query: `SELECT ${SPIKE} AS count` }),
    });
    await api.publishQuery(q.id);
    await executeAndWait(api, q.id);
    await evaluateAndWait(orgId, monitorId, NEED + 1);

    await expect
      .poll(() => monitorState(monitorId!), { timeout: 60_000, intervals: [1000] })
      .toBe("triggered");

    const statsRe = new RegExp(`Jumped to ${SPIKE} from a ${NEED}-check median of ${STEADY}`);

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
        { timeout: 90_000, intervals: [2000] },
      )
      .toContain("What changed");
    expect(body).toMatch(statsRe);

    await page.reload();
    const card = page.getByTestId("latest-incident");
    await expect(card).toBeVisible({ timeout: 15_000 });
    await expect(card).toContainText("Latest incident");
    await expect(card).toContainText(statsRe);
  } finally {
    if (monitorId) await api.deleteMonitor(monitorId).catch(() => {});
    await api.deleteQuery(q.id).catch(() => {});
    await api.deleteDataSource(ds.id).catch(() => {});
    await ctx.close();
  }
});
