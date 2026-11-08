import type { AiUsageResponse } from "@/types/api";

// The backend's four statuses collapse into three chart buckets: "invalid_key" and
// "quota_exceeded" both mean "fix the key", so they share a color.
export type Bucket = "ok" | "warn" | "error";
export const BUCKET_LABEL: Record<Bucket, string> = { ok: "OK", warn: "Quota / invalid key", error: "Failed" };
export const BUCKET_COLOR: Record<Bucket, string> = {
  ok: "var(--color-success)",
  warn: "var(--color-warning)",
  error: "var(--color-destructive)",
};

function bucketOf(status: string): Bucket {
  if (status === "ok") return "ok";
  if (status === "invalid_key" || status === "quota_exceeded") return "warn";
  return "error";
}

export function chartData(byDay: AiUsageResponse["by_day"]) {
  const byDate = new Map<string, Record<Bucket, number> & { date: string; cost: number | null }>();
  for (const row of byDay) {
    const entry = byDate.get(row.date) ?? { date: row.date, ok: 0, warn: 0, error: 0, cost: null };
    entry[bucketOf(row.status)] += row.count;
    if (row.cost_usd !== null) entry.cost = (entry.cost ?? 0) + row.cost_usd;
    byDate.set(row.date, entry);
  }
  return [...byDate.values()].sort((a, b) => a.date.localeCompare(b.date));
}
