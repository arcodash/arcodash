/** Shared numeric formatting for query result values, so a value never reads differently in two
 * places. */

// Grouping locale is pinned: mixing the browser's separator with the hardcoded "." decimal would
// yield e.g. de-DE "6.853.680.00".
export const NUMBER_LOCALE = "en-US";

/** Result-column friendly types rendered and charted as numbers. */
export function isNumericType(friendlyType: string): boolean {
  return friendlyType === "integer" || friendlyType === "float";
}

const DIGITS_ONLY = /^\d+$/;

/** Groups the integer part via BigInt so NUMERIC strings keep every digit; splits on the FIRST "."
 * so malformed multi-dot input is left unchanged. */
export function formatNumeric(value: number | string): string {
  const raw = typeof value === "number" ? value.toString() : value;
  const negative = raw.startsWith("-");
  const dot = raw.indexOf(".");
  const intPart = dot === -1 ? raw : raw.slice(0, dot);
  const digits = negative ? intPart.slice(1) : intPart;
  // BigInt("") is 0n, not an error — reject anything that isn't plain digits first.
  if (!DIGITS_ONLY.test(digits)) return raw;
  const fracPart = dot === -1 ? undefined : raw.slice(dot + 1);
  const grouped = BigInt(digits).toLocaleString(NUMBER_LOCALE);
  // Re-apply the sign BigInt(digits) stripped, keeping a signed "-0.00" from losing its "-".
  const signed = negative ? `-${grouped}` : grouped;
  return fracPart !== undefined ? `${signed}.${fracPart}` : signed;
}

/** Chart tick label: numeric-looking values are grouped; others are truncated so long categories
 * don't collide on a narrow axis. */
export function axisTick(value: unknown): string {
  const s =
    typeof value === "number" || (typeof value === "string" && /^-?\d/.test(value))
      ? formatNumeric(value as string | number)
      : String(value ?? "");
  return s.length > 14 ? `${s.slice(0, 13)}…` : s;
}

const NUMERIC_STRING = /^-?\d+(\.\d+)?$/;

/** Counter tile value: null -> "—"; non-numeric -> its plain string. Numeric strings stay strings
 * to keep every digit; auto decimals leave the fraction as-is, a fixed count pads/rounds. */
export function formatCounter(
  value: unknown,
  { grouping = true, decimals }: { grouping?: boolean; decimals?: number | null } = {},
): string {
  if (value === null || value === undefined) return "—";
  const numeric =
    typeof value === "number" ? Number.isFinite(value) : typeof value === "string" && NUMERIC_STRING.test(value);
  if (!numeric) return String(value);
  const n = value as number | string;
  if (decimals == null) return grouping ? formatNumeric(n) : String(n);
  return new Intl.NumberFormat(NUMBER_LOCALE, {
    useGrouping: grouping,
    minimumFractionDigits: decimals,
    maximumFractionDigits: decimals,
  }).format(n as number);
}

const USD = new Intl.NumberFormat(NUMBER_LOCALE, {
  style: "currency",
  currency: "USD",
  minimumFractionDigits: 2,
  maximumFractionDigits: 2,
});

/** Money for AI cost: null/undefined is unknown (never "$0.00"); sub-cent amounts read "<$0.01". */
export function formatUsd(v: number | null | undefined): string {
  if (v == null) return "—";
  if (v > 0 && v < 0.005) return "<$0.01";
  return USD.format(v);
}
