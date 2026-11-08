import type { AiErrorResponse } from "@/types/api";

/** Shared across every AI-backed endpoint's tagged response — only the "ok" shape differs between
 * them (Ask-with-AI, Auto Visualize, Suggest checks with AI). */
export function aiErrorMessage(res: AiErrorResponse): string {
  if (res.status === "rate_limited" && res.retry_after_secs != null) {
    return `${res.message} (try again in ${res.retry_after_secs}s)`;
  }
  return res.message;
}
