/** E2E: escalation chain + acknowledgment, waiting on real worker ticks (delay_minutes floor is 1,
 * so each step is ~60s). Requires: backend :8080, worker (redis :6380, MONITOR_TICK_SECS=5),
 * target postgres :5434, mailpit :8026/:1026. */
import { test, expect, type Page } from "@playwright/test";
import { adminContext } from "./helpers/auth";
import { loginAsAdmin, TARGET_DS_OPTIONS, executeAndWait } from "./helpers/api";
import { queryRows } from "./helpers/db";
import { enqueueEvaluateMonitor } from "./helpers/valkey";
import { MAILPIT_URL, type MailpitMessage, pollMailpitFor } from "./helpers/mailpit";

async function monitorState(monitorId: number): Promise<{ state: string; escalation_step: number }> {
  const rows = await queryRows<{ state: string; escalation_step: number }>(
    "SELECT state, escalation_step FROM monitors WHERE id = $1",
    [monitorId],
  );
  return rows[0];
}

test("escalation chain configured via UI fires after the tick delay; Acknowledge stops further escalation", async ({
  browser,
}, testInfo) => {
  testInfo.setTimeout(300_000);

  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page: Page = await ctx.newPage();

  const monitorName = `e2e-escalation-${Date.now()}`;
  const step1Address = `${monitorName}-step1@example.com`;
  const step2Address = `${monitorName}-step2@example.com`;

  const ds = await api.createDataSource({
    name: `e2e-escalation-ds-${Date.now()}`,
    type: "pg",
    options: TARGET_DS_OPTIONS,
  });
  const q = await api.createQuery({
    data_source_id: ds.id,
    // typed_table id=1 has int_col=42 (tests/e2e/seed/target-db.sql).
    name: `e2e-escalation-q-${Date.now()}`,
    query: "SELECT int_col AS count FROM typed_table WHERE id = 1",
  });
  await api.publishQuery(q.id); // monitor create requires a published query (require_published)
  const step1Dest = await api.createDestination({
    name: `e2e-escalation-step1-${Date.now()}`,
    type: "email",
    options: { to: step1Address },
  });
  const step2Dest = await api.createDestination({
    name: `e2e-escalation-step2-${Date.now()}`,
    type: "email",
    options: { to: step2Address },
  });
  const monitor = await api.createMonitor({
    query_id: q.id,
    name: monitorName,
    options: { column: "count", op: ">", value: 0 },
  });

  try {
    await page.goto(`/monitors/${monitor.id}`);
    await expect(page.getByText("No escalation steps")).toBeVisible({ timeout: 10_000 });
    // A background monitor refetch re-seeds the editor state (MonitorPage.tsx); let it settle
    // first.
    await page.waitForTimeout(1500);

    // Select order: query (0), operator (1), Notify (2), max-wait (3), subscribe destination (4),
    // then each escalation row's destination. Add and wire rows one at a time; a background refetch
    // can re-seed state between same-tick clicks.
    const comboboxes = page.getByRole("combobox");
    // Spinbutton order: "Value" (0), then each row's delay (1, 2); rows default to 30 min, so set
    // the 1-min floor.
    const spinbuttons = page.getByRole("spinbutton");

    await page.getByRole("button", { name: /\+ add step/i }).click();
    await expect(comboboxes).toHaveCount(7, { timeout: 10_000 });
    await spinbuttons.nth(1).fill("1");
    await comboboxes.nth(6).click();
    await page.getByRole("option", { name: `email: ${step1Dest.name}`, exact: true }).click();

    await page.getByRole("button", { name: /\+ add step/i }).click();
    await expect(comboboxes).toHaveCount(8, { timeout: 10_000 });
    await spinbuttons.nth(2).fill("1");
    await comboboxes.nth(7).click();
    await page.getByRole("option", { name: `email: ${step2Dest.name}`, exact: true }).click();

    await page.getByRole("button", { name: /^save escalation steps$/i }).click();
    await expect(page.getByText("Escalation steps saved.")).toBeVisible({ timeout: 10_000 });

    // Trigger for real; last_triggered_at is step 1's delay base.
    await executeAndWait(api, q.id);

    const orgRows = await queryRows<{ org_id: number }>("SELECT org_id FROM monitors WHERE id = $1", [
      monitor.id,
    ]);
    const orgId = orgRows[0].org_id;
    await enqueueEvaluateMonitor(orgId, monitor.id);

    await expect
      .poll(async () => (await monitorState(monitor.id)).state, { timeout: 60_000, intervals: [2000] })
      .toBe("triggered");

    // The escalation tick is floored at 60s plus up to 3s jitter, so worst case is ~60+60+3s plus
    // pickup; budget that plus slack.
    await expect
      .poll(async () => (await monitorState(monitor.id)).escalation_step, {
        timeout: 150_000,
        intervals: [3000],
      })
      .toBe(1);

    const step1Message = await pollMailpitFor(
      (m) => m.To?.some((t) => t.Address === step1Address),
      30_000,
    );
    expect(step1Message.Subject).toContain(monitorName);

    await page.reload();
    await page.getByRole("button", { name: /^acknowledge$/i }).click();
    await expect(page.getByText(/Acknowledged by .+ — escalation stopped\./)).toBeVisible({
      timeout: 10_000,
    });
    await expect(page.getByRole("button", { name: /^acknowledge$/i })).toHaveCount(0);

    // Wait past step 2's delay: escalation_step must never reach 2 and no step-2 email arrives.
    await new Promise((r) => setTimeout(r, 75_000));
    expect((await monitorState(monitor.id)).escalation_step).toBe(1);

    const mailpitRes = await fetch(`${MAILPIT_URL}/api/v1/messages?limit=100`);
    const mailpitBody = (await mailpitRes.json()) as { messages: MailpitMessage[] };
    const step2Sent = mailpitBody.messages?.some((m) => m.To?.some((t) => t.Address === step2Address));
    expect(step2Sent).toBe(false);
  } finally {
    await api.deleteMonitor(monitor.id).catch(() => {});
    await api.deleteDestination(step1Dest.id).catch(() => {});
    await api.deleteDestination(step2Dest.id).catch(() => {});
    await api.deleteQuery(q.id).catch(() => {});
    await api.deleteDataSource(ds.id).catch(() => {});
  }
});
