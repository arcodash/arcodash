/** Shared in-app validation for data source and destination forms (replaces the native bubble). Validate on
 * submit, then re-validate a field on change once it has an error. */

const URL_MESSAGE = "Enter a URL starting with http:// or https://.";
const EMAIL_MESSAGE = "Enter a valid email address.";
const PORT_MESSAGE = "Enter a port between 1 and 65535.";

export const EMAIL_RE = /^[^\s@]+@[^\s@]+\.[^\s@]+$/;
const URL_RE = /^https?:\/\//i;

export type FieldKind = "port" | "url" | "email";

/** Heuristic key to validation kind: `port` is a dedicated `integer` type; `url`/`email` are inferred from the key
 * since the schema only says `string`. */
export function fieldKind(key: string, schemaType: string | undefined): FieldKind | undefined {
  if (key === "port" || (schemaType === "integer" && /port/i.test(key))) return "port";
  if (/url/i.test(key)) return "url";
  if (key === "to" || key === "addresses" || /email/i.test(key)) return "email";
  return undefined;
}

/** `null` = valid. Required-but-empty wins over kind checks. */
export function validateValue(
  label: string,
  required: boolean,
  kind: FieldKind | undefined,
  raw: unknown,
): string | null {
  const value = raw == null ? "" : String(raw).trim();
  if (value === "") {
    return required ? `${label} is required.` : null;
  }
  switch (kind) {
    case "port": {
      const n = Number(value);
      return Number.isInteger(n) && n >= 1 && n <= 65535 ? null : PORT_MESSAGE;
    }
    case "url":
      return URL_RE.test(value) ? null : URL_MESSAGE;
    case "email":
      // "to" accepts a comma-separated list; every entry must look like an address.
      return value.split(",").every((part) => EMAIL_RE.test(part.trim()))
        ? null
        : EMAIL_MESSAGE;
    default:
      return null;
  }
}
