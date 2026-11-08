/** Shared date/time formatting for query result values. Granularity is computed once over a whole
 * value set, then applied to every value, so a date field never mixes "Jul 2022" with a raw
 * timestamp. */

export type DateGranularity = "month" | "day" | "datetime";

const LOCALE = "en-US";

function toDate(value: unknown): Date | null {
  if (!(value instanceof Date) && typeof value !== "string" && typeof value !== "number") return null;
  const d = new Date(value);
  return Number.isNaN(d.getTime()) ? null : d;
}

function isUtcMidnight(d: Date): boolean {
  return d.getUTCHours() === 0 && d.getUTCMinutes() === 0 && d.getUTCSeconds() === 0 && d.getUTCMilliseconds() === 0;
}

export function computeDateGranularity(values: Iterable<unknown>): DateGranularity {
  let sawAny = false;
  let allMidnight = true;
  let allFirstOfMonth = true;
  for (const raw of values) {
    const d = toDate(raw);
    if (!d) continue;
    sawAny = true;
    if (!isUtcMidnight(d)) allMidnight = false;
    if (d.getUTCDate() !== 1) allFirstOfMonth = false;
  }
  if (!sawAny) return "datetime";
  if (allMidnight && allFirstOfMonth) return "month";
  if (allMidnight) return "day";
  return "datetime";
}

const FORMATS: Record<DateGranularity, Intl.DateTimeFormat> = {
  month: new Intl.DateTimeFormat(LOCALE, { year: "numeric", month: "short", timeZone: "UTC" }),
  day: new Intl.DateTimeFormat(LOCALE, { year: "numeric", month: "short", day: "numeric", timeZone: "UTC" }),
  datetime: new Intl.DateTimeFormat(LOCALE, { dateStyle: "medium", timeStyle: "short" }),
};

/** Formats one value at a precomputed granularity. Month/day render in UTC (day-bucketed values are
 * UTC-midnight; local zone could shift them a calendar day); datetime renders in local time. */
export function formatDateValue(value: unknown, granularity: DateGranularity): string {
  const d = toDate(value);
  return d ? FORMATS[granularity].format(d) : String(value ?? "");
}
