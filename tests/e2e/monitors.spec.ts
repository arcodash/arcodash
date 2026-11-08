/** E2E: monitors. Requires: backend :8080, frontend :3000, target postgres :5434 */
import { test, expect, type Page } from "@playwright/test";
import { adminContext } from "./helpers/auth";
import { E2EApiClient, loginAsAdmin, TARGET_DS_OPTIONS, executeAndWait } from "./helpers/api";
import { countRows, queryRows } from "./helpers/db";
import { enqueueEvaluateMonitor } from "./helpers/valkey";
import { pollMailpitForSubject, fetchMailpitMessage } from "./helpers/mailpit";

/** Create a target-postgres data source + a published saved query; clean both up after. */
async function withSavedQuery(
  api: E2EApiClient,
  fn: (q: { id: number; name: string }) => Promise<void>,
) {
  const ds = await api.createDataSource({
    name: `e2e-monitor-ds-${Date.now()}`,
    type: "pg",
    options: TARGET_DS_OPTIONS,
  });
  const q = await api.createQuery({
    data_source_id: ds.id,
    name: `monitor-q-${Date.now()}`,
    query: "SELECT 1 AS count",
  });
  await api.publishQuery(q.id);
  try {
    await fn(q);
  } finally {
    await api.deleteQuery(q.id).catch(() => {});
    await api.deleteDataSource(ds.id).catch(() => {});
  }
}

async function pickQuery(page: Page, queryName: string) {
  await page.getByTestId("monitor-query-select").click();
  await page.getByRole("option", { name: queryName }).click();
}

async function pickColumn(page: Page, column: string) {
  await page.getByTestId("monitor-column-select").click();
  await page.getByRole("option", { name: column }).click();
}

test("create monitor via UI → appears in monitors list", async ({ browser }) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await withSavedQuery(api, async (q) => {
    const monitorName = `monitor-create-${Date.now()}`;
    await executeAndWait(api, q.id);
    await page.goto("/monitors/new");

    await page.fill('input[placeholder="Monitor name"]', monitorName);
    await pickQuery(page, q.name);
    await pickColumn(page, "count");

    await page.click('button:has-text("Save")');
    await page.waitForURL(/\/monitors\/\d+$/, { timeout: 10_000 });

    await page.goto("/monitors");
    await expect(page.getByText(monitorName)).toBeVisible({ timeout: 8000 });

    const rows = await queryRows<{ id: number }>(
      "SELECT id FROM monitors WHERE name = $1",
      [monitorName],
    );
    for (const r of rows) await api.deleteMonitor(Number(r.id)).catch(() => {});
  });

  await ctx.close();
});

test("new-monitor form defaults to self-subscribed → DB has an Email subscription and the detail page shows no banner", async ({
  browser,
}) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await withSavedQuery(api, async (q) => {
    const monitorName = `monitor-notify-default-${Date.now()}`;
    await executeAndWait(api, q.id);
    await page.goto("/monitors/new");

    await page.fill('input[placeholder="Monitor name"]', monitorName);
    await pickQuery(page, q.name);
    await pickColumn(page, "count");

    await expect(page.getByText("Email (just me)")).toBeVisible();

    await page.click('button:has-text("Save")');
    await page.waitForURL(/\/monitors\/\d+$/, { timeout: 10_000 });

    await expect(page.getByRole("heading", { name: monitorName })).toBeVisible({ timeout: 8000 });
    await expect(page.getByText("Edit Monitor")).toHaveCount(0);

    await expect(page.getByText(/won't notify anyone/i)).toHaveCount(0);

    const rows = await queryRows<{ id: number }>(
      "SELECT id FROM monitors WHERE name = $1",
      [monitorName],
    );
    expect(rows.length).toBe(1);
    const subCount = await countRows(
      "monitor_subscriptions",
      "monitor_id = $1 AND destination_id IS NULL",
      [rows[0].id],
    );
    expect(subCount).toBe(1);

    await api.deleteMonitor(Number(rows[0].id)).catch(() => {});
  });

  await ctx.close();
});

test("opting out of notify on create → monitor is saved with zero subscribers and the detail page shows the warning banner", async ({
  browser,
}) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await withSavedQuery(api, async (q) => {
    const monitorName = `monitor-notify-optout-${Date.now()}`;
    await executeAndWait(api, q.id);
    await page.goto("/monitors/new");

    await page.fill('input[placeholder="Monitor name"]', monitorName);
    await pickQuery(page, q.name);
    await pickColumn(page, "count");

    await page.getByRole("switch").click();
    await expect(page.getByText("Email (just me)")).toHaveCount(0);

    await page.click('button:has-text("Save")');
    await page.waitForURL(/\/monitors\/\d+$/, { timeout: 10_000 });

    await expect(
      page.getByText("No one is subscribed — this monitor won't notify anyone when it triggers."),
    ).toBeVisible({ timeout: 8000 });

    const rows = await queryRows<{ id: number }>(
      "SELECT id FROM monitors WHERE name = $1",
      [monitorName],
    );
    expect(rows.length).toBe(1);
    const subCount = await countRows("monitor_subscriptions", "monitor_id = $1", [rows[0].id]);
    expect(subCount).toBe(0);

    await api.deleteMonitor(Number(rows[0].id)).catch(() => {});
  });

  await ctx.close();
});

test("configure op/value + save → DB monitor.options reflects the comparison", async ({
  browser,
}) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await withSavedQuery(api, async (q) => {
    const monitorName = `monitor-opts-${Date.now()}`;
    await executeAndWait(api, q.id);
    await page.goto("/monitors/new");

    await page.fill('input[placeholder="Monitor name"]', monitorName);
    await pickQuery(page, q.name);
    await pickColumn(page, "count");

    // "<=" renders as "is at most" (OP_LABELS in lib/monitors.ts), not a bare symbol.
    await page.getByTestId("monitor-operator-select").click();
    await page.getByRole("option", { name: /is at most/i }).click();

    await page.fill('input[type="number"]', "100");

    await page.click('button:has-text("Save")');
    await page.waitForURL(/\/monitors\/\d+$/, { timeout: 10_000 });

    const rows = await queryRows<{ id: number; options: Record<string, unknown> }>(
      "SELECT id, options FROM monitors WHERE name = $1",
      [monitorName],
    );
    expect(rows.length).toBe(1);
    expect(rows[0].options.op).toBe("<=");
    expect(rows[0].options.value).toBe(100);
    expect(rows[0].options.column).toBe("count");

    await api.deleteMonitor(Number(rows[0].id)).catch(() => {});
  });

  await ctx.close();
});

test("mute monitor in UI → button flips to Unmute and options.muted=true in DB", async ({
  browser,
}) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await withSavedQuery(api, async (q) => {
    const monitorName = `monitor-mute-${Date.now()}`;
    const created = await api.createMonitor({
      query_id: q.id,
      name: monitorName,
      options: { column: "count", op: ">", value: 0 },
    });
    try {
      await page.goto(`/monitors/${created.id}`);

      const muteBtn = page.getByRole("button", { name: /^(Mute|Unmute)$/ });
      await expect(muteBtn).toHaveText("Mute", { timeout: 8000 });

      await muteBtn.click();
      await expect(
        page.getByRole("button", { name: "Unmute" }),
      ).toBeVisible({ timeout: 8000 });

      const rows = await queryRows<{ options: Record<string, unknown> }>(
        "SELECT options FROM monitors WHERE id = $1",
        [created.id],
      );
      expect(rows[0].options.muted).toBe(true);
    } finally {
      await api.deleteMonitor(created.id).catch(() => {});
    }
  });

  await ctx.close();
});

test("snooze monitor in UI → button flips to Cancel snooze, persists across reload, and DB options.muted_until clears on cancel", async ({
  browser,
}) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await withSavedQuery(api, async (q) => {
    const monitorName = `monitor-snooze-${Date.now()}`;
    const created = await api.createMonitor({
      query_id: q.id,
      name: monitorName,
      options: { column: "count", op: ">", value: 0 },
    });
    try {
      await page.goto(`/monitors/${created.id}`);

      const snoozeBtn = page.getByRole("button", { name: "Snooze" });
      await expect(snoozeBtn).toBeVisible({ timeout: 8000 });
      await snoozeBtn.click();
      await page.getByRole("menuitem", { name: "1 hour" }).click();

      await expect(
        page.getByRole("button", { name: "Cancel snooze" }),
      ).toBeVisible({ timeout: 8000 });
      await expect(page.getByText(/^Snoozed until .+ — notifications paused\.$/)).toBeVisible();

      await page.reload();
      await expect(
        page.getByRole("button", { name: "Cancel snooze" }),
      ).toBeVisible({ timeout: 8000 });
      await expect(page.getByText(/^Snoozed until .+ — notifications paused\.$/)).toBeVisible();

      await page.getByRole("button", { name: "Cancel snooze" }).click();
      await expect(page.getByRole("button", { name: "Snooze" })).toBeVisible({ timeout: 8000 });

      // "Snooze" appears only after the cancel refetch lands (no optimistic update), and the test's
      // DB connection can lag; poll.
      await expect
        .poll(
          async () => {
            const rows = await queryRows<{ options: Record<string, unknown> }>(
              "SELECT options FROM monitors WHERE id = $1",
              [created.id],
            );
            return rows[0].options.muted_until;
          },
          { timeout: 8_000, intervals: [250, 500, 1000] },
        )
        .toBeNull();
    } finally {
      await api.deleteMonitor(created.id).catch(() => {});
    }
  });

  await ctx.close();
});

test("rearm 'at most every N minutes' round-trips through save/reload and DB monitors.rearm", async ({
  browser,
}) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await withSavedQuery(api, async (q) => {
    const monitorName = `monitor-rearm-${Date.now()}`;
    const created = await api.createMonitor({
      query_id: q.id,
      name: monitorName,
      options: { column: "count", op: ">", value: 0 },
    });
    try {
      await page.goto(`/monitors/${created.id}`);
      await expect(page.getByPlaceholder("Monitor name")).toHaveValue(monitorName, {
        timeout: 8000,
      });

      await page.getByTestId("monitor-rearm-mode-select").click();
      await page.getByRole("option", { name: "At most every N minutes" }).click();

      const minutesInput = page.getByRole("spinbutton").nth(1);
      await minutesInput.fill("45");

      // Wait for the save response, not the "Saving…" label: isPending can settle first, and an
      // early reload cancels the in-flight POST.
      const savePost = page.waitForResponse(
        (res) => res.url().includes("/api/monitors/") && res.request().method() === "POST",
      );
      await page.click('button:has-text("Save")');
      await savePost;
      await expect(page.getByText("Saving…")).toHaveCount(0, { timeout: 8000 });

      await page.reload();
      await expect(page.getByPlaceholder("Monitor name")).toHaveValue(monitorName, {
        timeout: 8000,
      });
      await expect(page.getByText("At most every N minutes")).toBeVisible();
      await expect(page.getByRole("spinbutton").nth(1)).toHaveValue("45");

      const rows = await queryRows<{ rearm: number }>(
        "SELECT rearm FROM monitors WHERE id = $1",
        [created.id],
      );
      expect(Number(rows[0].rearm)).toBe(45 * 60);
    } finally {
      await api.deleteMonitor(created.id).catch(() => {});
    }
  });

  await ctx.close();
});

test("add a notification subscription via UI → subscription row appears + DB row exists", async ({
  browser,
}) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await withSavedQuery(api, async (q) => {
    const monitorName = `monitor-sub-${Date.now()}`;
    const created = await api.createMonitor({
      query_id: q.id,
      name: monitorName,
      options: { column: "count", op: ">", value: 0 },
    });
    try {
      await page.goto(`/monitors/${created.id}`);

      await expect(page.getByText("No subscriptions yet")).toBeVisible({
        timeout: 8000,
      });

      await page.getByRole("button", { name: "Subscribe" }).click();

      await expect(
        page.getByRole("cell", { name: "Email" }),
      ).toBeVisible({ timeout: 8000 });

      const subCount = await countRows(
        "monitor_subscriptions",
        "monitor_id = $1",
        [created.id],
      );
      expect(subCount).toBe(1);
    } finally {
      await api.deleteMonitor(created.id).catch(() => {});
    }
  });

  await ctx.close();
});

test("subscribe via a real destination in UI → channel cell + DB destination_id both reflect it", async ({
  browser,
}) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await withSavedQuery(api, async (q) => {
    const monitorName = `monitor-sub-dest-${Date.now()}`;
    // type "email": subscribing only stores an FK, no live receiver needed.
    const destName = `e2e-monitor-dest-${Date.now()}`;
    const dest = await api.createDestination({
      name: destName,
      type: "email",
      options: { to: "e2e-monitor-dest@example.com" },
    });
    const created = await api.createMonitor({
      query_id: q.id,
      name: monitorName,
      options: { column: "count", op: ">", value: 0 },
    });
    try {
      await page.goto(`/monitors/${created.id}`);

      await expect(page.getByText("No subscriptions yet")).toBeVisible({
        timeout: 8000,
      });

      await page.getByTestId("monitor-subscribe-destination-select").click();
      await page.getByRole("option", { name: `email: ${destName}`, exact: true }).click();
      await page.getByRole("button", { name: "Subscribe" }).click();

      await expect(
        page.getByRole("cell", { name: `email: ${destName}` }),
      ).toBeVisible({ timeout: 8000 });

      // destination_id is BIGINT; node-pg returns it as a string.
      const rows = await queryRows<{ destination_id: string | null }>(
        "SELECT destination_id FROM monitor_subscriptions WHERE monitor_id = $1",
        [created.id],
      );
      expect(rows.length).toBe(1);
      expect(Number(rows[0].destination_id)).toBe(dest.id);
    } finally {
      await api.deleteMonitor(created.id).catch(() => {});
      await api.deleteDestination(dest.id).catch(() => {});
    }
  });

  await ctx.close();
});

test("click Subscribe twice with same destination → no duplicate row, no error shown", async ({
  browser,
}) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await withSavedQuery(api, async (q) => {
    const monitorName = `monitor-sub-dup-${Date.now()}`;
    const created = await api.createMonitor({
      query_id: q.id,
      name: monitorName,
      options: { column: "count", op: ">", value: 0 },
    });
    try {
      await page.goto(`/monitors/${created.id}`);

      await expect(page.getByText("No subscriptions yet")).toBeVisible({
        timeout: 8000,
      });

      await page.getByRole("button", { name: "Subscribe" }).click();
      await expect(
        page.getByRole("cell", { name: "Email" }),
      ).toBeVisible({ timeout: 8000 });

      // Re-subscribing with the same destination returns 409, which the page treats as a silent
      // no-op.
      await page.getByRole("button", { name: "Subscribe" }).click();
      await expect(page.getByRole("button", { name: "Subscribe" })).toBeEnabled({
        timeout: 8000,
      });

      await expect(page.locator("p.text-destructive")).toHaveCount(0);
      await expect(page.getByRole("cell", { name: "Email" })).toHaveCount(1);

      const subCount = await countRows(
        "monitor_subscriptions",
        "monitor_id = $1 AND destination_id IS NULL",
        [created.id],
      );
      expect(subCount).toBe(1);
    } finally {
      await api.deleteMonitor(created.id).catch(() => {});
    }
  });

  await ctx.close();
});

test("delete monitor in UI → disappears from list and DB row gone", async ({
  browser,
}) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await withSavedQuery(api, async (q) => {
    const monitorName = `monitor-del-${Date.now()}`;
    const created = await api.createMonitor({
      query_id: q.id,
      name: monitorName,
      options: { column: "count", op: ">", value: 0 },
    });
    let deletedViaUi = false;
    try {
      await page.goto(`/monitors/${created.id}`);

      await page.getByRole("button", { name: "Delete monitor" }).click();
      await page.getByRole("button", { name: "Delete", exact: true }).click();

      await page.waitForURL(/\/monitors$/, { timeout: 10_000 });
      await expect(page.getByText(monitorName)).not.toBeVisible();

      const remaining = await countRows("monitors", "id = $1", [created.id]);
      expect(remaining).toBe(0);
      deletedViaUi = true;
    } finally {
      if (!deletedViaUi) await api.deleteMonitor(created.id).catch(() => {});
    }
  });

  await ctx.close();
});

test("delivery history panel: renders a sent + a failed badge, and 'Last notified' reflects only the successful send", async ({
  browser,
}, testInfo) => {
  testInfo.setTimeout(120_000);

  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await withSavedQuery(api, async (q) => {
    const monitorName = `monitor-deliveries-${Date.now()}`;

    // Real Mailpit send alongside a webhook with no url: a permanent config error, never retried. A
    // closed port would be transient and retried, making ["failed","sent"] flaky.
    const emailDest = await api.createDestination({
      name: `deliveries-email-${Date.now()}`,
      type: "email",
      options: { to: `${monitorName}@example.com` },
    });
    const webhookDest = await api.createDestination({
      name: `deliveries-webhook-${Date.now()}`,
      type: "webhook",
      options: {},
    });

    const monitor = await api.createMonitor({
      query_id: q.id,
      name: monitorName,
      options: { column: "count", op: ">", value: 0 },
    });

    try {
      await api.createMonitorSubscription(monitor.id, { destination_id: emailDest.id });
      await api.createMonitorSubscription(monitor.id, { destination_id: webhookDest.id });

      await executeAndWait(api, q.id);
      const orgRows = await queryRows<{ org_id: number }>(
        "SELECT org_id FROM monitors WHERE id = $1",
        [monitor.id],
      );
      await enqueueEvaluateMonitor(orgRows[0].org_id, monitor.id);

      await expect
        .poll(
          async () => {
            const r = await queryRows<{ status: string }>(
              "SELECT status FROM notification_deliveries WHERE monitor_id = $1",
              [monitor.id],
            );
            return r.map((row) => row.status).sort();
          },
          { timeout: 90_000, intervals: [2000] },
        )
        .toEqual(["failed", "sent"]);

      const sentRows = await queryRows<{ created_at: string }>(
        "SELECT created_at FROM notification_deliveries WHERE monitor_id = $1 AND status = 'sent'",
        [monitor.id],
      );
      const sentAt = new Date(sentRows[0].created_at).getTime();

      await page.goto(`/monitors/${monitor.id}`);

      await expect(page.getByText("Sent", { exact: true })).toBeVisible({ timeout: 8000 });
      await expect(page.getByText("Failed", { exact: true })).toBeVisible({ timeout: 8000 });

      // "Last notified" keys off the sent row, never the failed one.
      const lastNotified = page.getByText(/^Last notified /);
      await expect(lastNotified).toBeVisible({ timeout: 8000 });
      const text = await lastNotified.textContent();
      const displayed = new Date(text!.replace("Last notified ", "")).getTime();
      expect(Math.abs(displayed - sentAt)).toBeLessThan(5000);
      await expect(page.getByText("Not notified yet")).not.toBeVisible();
    } finally {
      await api.deleteMonitor(monitor.id).catch(() => {});
      await api.deleteDestination(emailDest.id).catch(() => {});
      await api.deleteDestination(webhookDest.id).catch(() => {});
    }
  });

  await ctx.close();
});

test("add a destination to an already-triggered monitor → firing note shown, real email arrives", async ({
  browser,
}, testInfo) => {
  testInfo.setTimeout(120_000);

  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await withSavedQuery(api, async (q) => {
    const monitorName = `monitor-notify-sub-${Date.now()}`;
    const destName = `e2e-notify-sub-dest-${Date.now()}`;

    const monitor = await api.createMonitor({
      query_id: q.id,
      name: monitorName,
      options: { column: "count", op: ">", value: 0 },
    });

    try {
      await executeAndWait(api, q.id);
      const orgRows = await queryRows<{ org_id: number }>(
        "SELECT org_id FROM monitors WHERE id = $1",
        [monitor.id],
      );
      await enqueueEvaluateMonitor(orgRows[0].org_id, monitor.id);
      await expect
        .poll(
          async () => {
            const r = await queryRows<{ state: string }>(
              "SELECT state FROM monitors WHERE id = $1",
              [monitor.id],
            );
            return r[0]?.state;
          },
          { timeout: 90_000, intervals: [2000] },
        )
        .toBe("triggered");

      const dest = await api.createDestination({
        name: destName,
        type: "email",
        options: { to: `${monitorName}@example.com` },
      });

      await page.goto(`/monitors/${monitor.id}`);

      await expect(
        page.getByText(
          "This monitor is currently alerting. Anyone you subscribe is notified right away.",
        ),
      ).toBeVisible({ timeout: 8000 });

      await page.getByTestId("monitor-subscribe-destination-select").click();
      await page.getByRole("option", { name: `email: ${destName}`, exact: true }).click();
      await page.getByRole("button", { name: "Subscribe" }).click();
      // .first(): the same text can also appear in the Delivery history panel below.
      await expect(page.getByRole("cell", { name: `email: ${destName}` }).first()).toBeVisible({
        timeout: 8000,
      });

      const message = await pollMailpitForSubject(monitorName, 90_000);
      expect(message.Subject).toContain(monitorName);
      expect(message.Subject.toLowerCase()).toContain("alerting");

      // Delivery history is refetched at subscribe time, before the worker writes the row; poll
      // with reloads.
      await expect(async () => {
        await page.reload();
        await expect(page.getByText("Sent", { exact: true })).toBeVisible({ timeout: 2000 });
      }).toPass({ timeout: 15_000, intervals: [1000] });

      await api.deleteDestination(dest.id).catch(() => {});
    } finally {
      await api.deleteMonitor(monitor.id).catch(() => {});
    }
  });

  await ctx.close();
});

test("monitor with custom subject/body → real triggered email carries the substituted copy", async ({}, testInfo) => {
  testInfo.setTimeout(120_000);

  const api = await loginAsAdmin();

  await withSavedQuery(api, async (q) => {
    const monitorName = `monitor-custom-copy-${Date.now()}`;
    const toAddress = `${monitorName}@example.com`;

    const destination = await api.createDestination({
      name: `e2e-custom-copy-dest-${Date.now()}`,
      type: "email",
      options: { to: toAddress },
    });
    const monitor = await api.createMonitor({
      query_id: q.id,
      name: monitorName,
      options: {
        column: "count",
        op: ">",
        value: 0,
        custom_subject: `CUSTOM SUBJECT {{name}} / {{state}}`,
        custom_body: `CUSTOM BODY for {{name}}, now {{state}}.`,
      },
    });

    try {
      await api.createMonitorSubscription(monitor.id, { destination_id: destination.id });

      await executeAndWait(api, q.id);
      const orgRows = await queryRows<{ org_id: number }>(
        "SELECT org_id FROM monitors WHERE id = $1",
        [monitor.id],
      );
      await enqueueEvaluateMonitor(orgRows[0].org_id, monitor.id);
      await expect
        .poll(
          async () => {
            const r = await queryRows<{ state: string }>(
              "SELECT state FROM monitors WHERE id = $1",
              [monitor.id],
            );
            return r[0]?.state;
          },
          { timeout: 90_000, intervals: [2000] },
        )
        .toBe("triggered");

      // {{state}} substitutes the display label (state_label in send_notification.rs), not the raw
      // DB value.
      const expectedSubject = `CUSTOM SUBJECT ${monitorName} / Alerting`;
      const found = await pollMailpitForSubject(expectedSubject, 90_000);
      expect(found.Subject).toBe(expectedSubject);

      // A custom body replaces only the intro slot; facts table, CTA and footer stay fixed.
      const full = await fetchMailpitMessage(found.ID);
      expect(full.Text).toContain(`CUSTOM BODY for ${monitorName}, now Alerting.`);

      await api.deleteDestination(destination.id).catch(() => {});
    } finally {
      await api.deleteMonitor(monitor.id).catch(() => {});
    }
  });
});

test('"Send now" on an existing subscription → queued confirmation shown, real email arrives', async ({
  browser,
}, testInfo) => {
  testInfo.setTimeout(90_000);

  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await withSavedQuery(api, async (q) => {
    const monitorName = `monitor-send-now-${Date.now()}`;
    const destName = `e2e-send-now-dest-${Date.now()}`;

    // High threshold: monitor stays "unknown"; "Send now" must work in any state.
    const monitor = await api.createMonitor({
      query_id: q.id,
      name: monitorName,
      options: { column: "count", op: ">", value: 1000 },
    });
    const dest = await api.createDestination({
      name: destName,
      type: "email",
      options: { to: `${monitorName}@example.com` },
    });

    try {
      await api.createMonitorSubscription(monitor.id, { destination_id: dest.id });

      await page.goto(`/monitors/${monitor.id}`);
      await expect(page.getByRole("cell", { name: `email: ${destName}` })).toBeVisible({
        timeout: 8000,
      });
      await expect(
        page.getByText(/currently alerting\. Anyone you subscribe is notified right away/i),
      ).not.toBeVisible();

      await page.getByRole("button", { name: "Send now" }).click();
      await expect(
        page.getByText("Notification queued — the result appears in Delivery history below."),
      ).toBeVisible({ timeout: 8000 });

      // Subject carries the current state ("Not checked yet" for unknown), not a hardcoded
      // "triggered".
      const message = await pollMailpitForSubject(monitorName, 60_000);
      expect(message.Subject).toContain(monitorName);
      expect(message.Subject.toLowerCase()).toContain("not checked yet");

      // Delivery history refetches when the job is queued, before the worker finishes; reload to
      // pick up the send.
      await page.reload();
      await expect(page.getByText("Sent", { exact: true })).toBeVisible({ timeout: 8000 });
    } finally {
      await api.deleteMonitor(monitor.id).catch(() => {});
      await api.deleteDestination(dest.id).catch(() => {});
    }
  });

  await ctx.close();
});

test("query's Monitors card round-trips through Add-monitor → create → shows state → links back", async ({
  browser,
}) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await withSavedQuery(api, async (q) => {
    const monitorName = `monitor-crosslink-${Date.now()}`;
    let created: { id: number } | null = null;

    try {
      await executeAndWait(api, q.id);
      await page.goto(`/queries/${q.id}`);

      await expect(page.getByText("No monitors watch this query")).toBeVisible({
        timeout: 8000,
      });

      await page.getByRole("button", { name: "Add monitor" }).click();
      await page.waitForURL(new RegExp(`/monitors/new\\?query_id=${q.id}$`), {
        timeout: 10_000,
      });

      await expect(page.getByTestId("monitor-query-select")).toContainText(q.name);

      await page.fill('input[placeholder="Monitor name"]', monitorName);
      await pickColumn(page, "count");
      await page.click('button:has-text("Save")');
      await page.waitForURL(/\/monitors\/\d+$/, { timeout: 10_000 });

      const rows = await queryRows<{ id: number }>(
        "SELECT id FROM monitors WHERE name = $1",
        [monitorName],
      );
      expect(rows.length).toBe(1);
      created = { id: Number(rows[0].id) };

      // New monitor appears with a self link, "—" for enabled, "Not checked yet" for state.
      await page.goto(`/queries/${q.id}`);
      const monitorLink = page.getByRole("link", { name: monitorName });
      await expect(monitorLink).toBeVisible({ timeout: 8000 });
      await expect(monitorLink).toHaveAttribute("href", `/monitors/${created.id}`);
      await expect(page.getByText("Not checked yet", { exact: true })).toBeVisible();

      await monitorLink.click();
      await page.waitForURL(new RegExp(`/monitors/${created.id}$`), { timeout: 8000 });

      const readsLink = page.getByRole("link", { name: q.name });
      await expect(readsLink).toBeVisible({ timeout: 8000 });
      await readsLink.click();
      await page.waitForURL(new RegExp(`/queries/${q.id}$`), { timeout: 8000 });
    } finally {
      if (created) await api.deleteMonitor(created.id).catch(() => {});
    }
  });

  await ctx.close();
});

test("auth boundary: unauthenticated GET /api/monitors → 401", async () => {
  const anon = new E2EApiClient();
  const status = await anon.getStatus("/api/monitors");
  expect(status).toBe(401);
});

test("monitor column field is a picker over the query's real result columns, not free text", async ({
  browser,
}) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  const monitorName = `monitor-colpicker-${Date.now()}`;

  await withSavedQuery(api, async (q) => {
    await executeAndWait(api, q.id);

    await page.goto("/monitors/new");
    await page.fill('input[placeholder="Monitor name"]', monitorName);
    await pickQuery(page, q.name);

    // Single-option Select: a Radix positioning regression (item-aligned vs popper) overlays the
    // item on the trigger; assert committed state (aria-expanded false, text updated), then Save.
    await expect(page.locator('input[placeholder="e.g. count"]')).toHaveCount(0);
    const columnSelect = page.getByTestId("monitor-column-select");
    await expect(columnSelect).toBeVisible({ timeout: 8000 });
    await columnSelect.click();
    await expect(page.getByRole("option", { name: "count" })).toBeVisible();
    await page.getByRole("option", { name: "count" }).click();
    await expect(columnSelect).toHaveAttribute("aria-expanded", "false");
    await expect(columnSelect).toHaveText("count");

    const saveButton = page.locator('button:has-text("Save")');
    await expect(saveButton).toBeEnabled();
    await saveButton.click();
    await page.waitForURL(/\/monitors\/\d+$/, { timeout: 10_000 });

    const rows = await queryRows<{ id: number; options: { column?: string } }>(
      "SELECT id, options FROM monitors WHERE name = $1",
      [monitorName],
    );
    expect(rows.length).toBe(1);
    expect(rows[0].options.column).toBe("count");
    await api.deleteMonitor(Number(rows[0].id)).catch(() => {});
  });

  await ctx.close();
});

test("a monitor watching a column absent from the result surfaces its own reason, not a bare unknown", async ({
  browser,
}, testInfo) => {
  testInfo.setTimeout(60_000);

  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await withSavedQuery(api, async (q) => {
    const monitor = await api.createMonitor({
      query_id: q.id,
      name: `monitor-unevaluable-${Date.now()}`,
      options: { column: "revenue", op: ">", value: 0 },
    });

    try {
      await executeAndWait(api, q.id);
      const orgRows = await queryRows<{ org_id: number }>(
        "SELECT org_id FROM monitors WHERE id = $1",
        [monitor.id],
      );
      await enqueueEvaluateMonitor(orgRows[0].org_id, monitor.id);

      await expect
        .poll(
          async () => {
            const r = await queryRows<{ state: string; unevaluable_reason: string | null }>(
              "SELECT state, unevaluable_reason FROM monitors WHERE id = $1",
              [monitor.id],
            );
            return r[0];
          },
          { timeout: 20_000 },
        )
        .toMatchObject({ state: "unevaluable" });

      const rows = await queryRows<{ unevaluable_reason: string | null }>(
        "SELECT unevaluable_reason FROM monitors WHERE id = $1",
        [monitor.id],
      );
      expect(rows[0].unevaluable_reason).toContain("revenue");

      await page.goto(`/monitors/${monitor.id}`);
      await expect(page.getByText(/Can.t evaluate:.*revenue/i)).toBeVisible({ timeout: 8000 });
    } finally {
      await api.deleteMonitor(monitor.id).catch(() => {});
    }
  });

  await ctx.close();
});

test("a save is refused once the monitor changed since it was opened (two tabs)", async ({ browser }) => {
  const api = await loginAsAdmin();

  await withSavedQuery(api, async (q) => {
    const monitorName = `monitor-conflict-${Date.now()}`;
    const monitor = await api.createMonitor({
      query_id: q.id,
      name: monitorName,
      options: { column: "count", op: ">", value: 0 },
    });

    // Two separate sessions; the second never reloads, so it keeps its stale snapshot.
    const ctxA = await adminContext(browser);
    const ctxB = await adminContext(browser);
    const pageA = await ctxA.newPage();
    const pageB = await ctxB.newPage();

    try {
      await pageA.goto(`/monitors/${monitor.id}`);
      await pageB.goto(`/monitors/${monitor.id}`);
      await expect(pageA.locator("#monitor-value")).toHaveValue("0", { timeout: 8000 });
      await expect(pageB.locator("#monitor-value")).toHaveValue("0", { timeout: 8000 });

      await pageA.fill("#monitor-value", "5");
      // A's save must land before B saves, or B could win.
      await Promise.all([
        pageA.waitForResponse(
          (r) => r.url().endsWith(`/api/monitors/${monitor.id}`) && r.request().method() === "POST" && r.ok(),
        ),
        pageA.click('button:has-text("Save")'),
      ]);
      await expect(pageA.getByRole("button", { name: "Save", exact: true })).toBeEnabled({ timeout: 8000 });
      await expect(pageA.locator("p.text-destructive")).toHaveCount(0);

      // B's stale save is refused with the conflict banner.
      await pageB.fill("#monitor-value", "9");
      await pageB.click('button:has-text("Save")');
      await expect(pageB.getByTestId("monitor-conflict-banner")).toBeVisible({ timeout: 8000 });

      const fresh = await api.getMonitor(monitor.id);
      expect(fresh.options.value).toBe(5);

      await pageB.getByRole("button", { name: "Reload", exact: true }).click();
      await expect(pageB.locator("#monitor-value")).toHaveValue("5", { timeout: 8000 });
      await expect(pageB.getByTestId("monitor-conflict-banner")).toHaveCount(0);

      await pageB.fill("#monitor-value", "9");
      await pageB.click('button:has-text("Save")');
      await expect(pageB.getByTestId("monitor-conflict-banner")).toHaveCount(0);
      await expect
        .poll(async () => (await api.getMonitor(monitor.id)).options.value, { timeout: 8000 })
        .toBe(9);
    } finally {
      await api.deleteMonitor(monitor.id).catch(() => {});
      await ctxA.close();
      await ctxB.close();
    }
  });
});

test("muting then saving does not fabricate a conflict (real backend updated_at bump)", async ({
  browser,
}) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await withSavedQuery(api, async (q) => {
    const monitorName = `monitor-mute-save-${Date.now()}`;
    const monitor = await api.createMonitor({
      query_id: q.id,
      name: monitorName,
      options: { column: "count", op: ">", value: 0 },
    });

    try {
      await page.goto(`/monitors/${monitor.id}`);
      await expect(page.locator("#monitor-value")).toHaveValue("0", { timeout: 8000 });

      // Mute bumps updated_at without touching form fields; the freshness check must ignore it or
      // the save 409s.
      await page.getByRole("button", { name: "Mute" }).click();
      await expect(page.getByRole("button", { name: "Unmute" })).toBeVisible({ timeout: 8000 });

      await page.fill("#monitor-value", "7");
      await page.click('button:has-text("Save")');
      await expect(page.getByTestId("monitor-conflict-banner")).toHaveCount(0);

      // The mute must survive the save (options are merged, not replaced).
      await expect
        .poll(async () => (await api.getMonitor(monitor.id)).options.value, { timeout: 8000 })
        .toBe(7);
      expect((await api.getMonitor(monitor.id)).options.muted).toBe(true);
    } finally {
      await api.deleteMonitor(monitor.id).catch(() => {});
      await ctx.close();
    }
  });
});
