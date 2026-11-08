import { useCallback, useEffect, useReducer, useRef } from "react";
import { api, apiErrorMessage, ApiError } from "@/lib/api";
import { aiErrorMessage } from "@/lib/ai";
import type { AskEvent } from "@/types/api";

export type AskStep =
  | { kind: "status"; text: string }
  | { kind: "draft"; attempt: number; sql: string }
  | { kind: "executing"; attempt: number }
  | { kind: "repair"; attempt: number; error: string }
  | { kind: "preview"; shown: number; rowCount: number; truncated: boolean }
  | { kind: "refreshing" }
  | { kind: "retrying" }
  | { kind: "visualizing" }
  | { kind: "suggesting" };

export interface AskPreview {
  columns: { name: string; friendly_type: string }[];
  rows: unknown[];
  rowCount: number;
  truncated: boolean;
}

export type AskSource = "ask" | "fix" | "dashboard" | "monitors_page";

export interface AskRunState {
  phase: "idle" | "running" | "done" | "failed" | "cancelled";
  runId: number;
  source: AskSource;
  chained: boolean;
  created: Extract<AskEvent, { type: "created" }> | null;
  suggested: Extract<AskEvent, { type: "suggested" }> | null;
  steps: AskStep[];
  latestSql: string | null;
  preview: AskPreview | null;
  succeeded: boolean;
  final: Extract<AskEvent, { type: "final" }> | null;
  error: string | null;
  /** Reason for a failed run: the `failed` event status, or "error" for transport errors. */
  failReason: string | null;
  /** True while the data source schema is refreshing before the single automatic re-ask. */
  waiting: boolean;
  autoRetried: boolean;
  /** Failure that is slowness, not breakage: rendered as a warning. */
  giveUp: boolean;
}

export const IDLE_RUN: AskRunState = {
  phase: "idle",
  runId: 0,
  source: "ask",
  chained: false,
  created: null,
  suggested: null,
  steps: [],
  latestSql: null,
  preview: null,
  succeeded: false,
  final: null,
  error: null,
  failReason: null,
  waiting: false,
  autoRetried: false,
  giveUp: false,
};

const POLL_MS = 3000;
export const REFRESH_WAIT_CAP_MS = 60_000;
const REFRESH_FAILED_MESSAGE =
  "The schema couldn't be updated. Ask an admin to check this data source.";

type Action =
  | { type: "start"; runId: number; source: AskSource }
  | { type: "event"; event: AskEvent }
  | { type: "fail"; message: string; reason?: string }
  | { type: "retry" }
  | { type: "wait_failed"; message: string | null }
  | { type: "cancel" }
  | { type: "reset" };

function reduce(state: AskRunState, action: Action): AskRunState {
  switch (action.type) {
    case "start":
      return {
        ...IDLE_RUN,
        phase: "running",
        runId: action.runId,
        source: action.source,
        chained: action.source === "dashboard" || action.source === "monitors_page",
      };
    case "reset":
      return { ...IDLE_RUN, runId: state.runId };
    case "cancel":
      return state.phase === "running" ? { ...state, phase: "cancelled", waiting: false } : state;
    case "retry":
      return state.waiting
        ? { ...state, waiting: false, autoRetried: true, steps: [...state.steps, { kind: "retrying" }] }
        : state;
    case "wait_failed":
      return state.waiting
        ? {
            ...state,
            phase: "failed",
            waiting: false,
            error: action.message ?? "The schema is still updating. Try again in a minute.",
            failReason: action.message ? "profile_refresh_failed" : "profile_refreshing_timeout",
            giveUp: !action.message,
          }
        : state;
    case "fail":
      return state.phase === "running" && !state.waiting ? { ...state, phase: "failed", error: action.message, failReason: action.reason ?? "error" } : state;
    case "event": {
      if (state.phase !== "running" || state.waiting) return state;
      const e = action.event;
      const push = (step: AskStep, patch?: Partial<AskRunState>): AskRunState => ({
        ...state,
        ...patch,
        steps: [...state.steps, step],
      });
      switch (e.type) {
        case "status":
          return push({ kind: "status", text: e.text });
        case "draft":
          return push({ kind: "draft", attempt: e.attempt, sql: e.sql }, { latestSql: e.sql, preview: null, succeeded: false });
        case "executing":
          return push({ kind: "executing", attempt: e.attempt });
        case "preview":
          return push(
            { kind: "preview", shown: e.rows.length, rowCount: e.row_count, truncated: e.truncated },
            {
              succeeded: true,
              preview: { columns: e.columns, rows: e.rows, rowCount: e.row_count, truncated: e.truncated },
            },
          );
        case "repair":
          return push({ kind: "repair", attempt: e.attempt, error: e.error }, { succeeded: false });
        case "profile_refreshing":
          return state.autoRetried ? state : push({ kind: "refreshing" }, { waiting: true });
        case "visualizing":
        case "suggesting":
          return push({ kind: e.type });
        case "suggested":
          return { ...state, phase: "done", suggested: e };
        case "created":
          return { ...state, phase: "done", created: e };
        case "final":
          return { ...state, phase: state.chained ? "running" : "done", final: e, latestSql: e.sql, succeeded: e.executed ?? state.succeeded };
        case "failed":
          return { ...state, phase: "failed", error: aiErrorMessage(e), failReason: e.status };
      }
    }
  }
}

/** A dropped/aborted stream is a TypeError that `apiErrorMessage` would flatten to the generic
 * fallback; say what happened. */
export function askFailureMessage(err: unknown): string {
  if (err instanceof TypeError) {
    return "The connection to the AI dropped before it finished. Try again.";
  }
  return apiErrorMessage(err, "Failed to generate SQL");
}

export function failureReason(err: unknown): string {
  if (err instanceof ApiError) return err.errorCode ?? (err.status === 403 ? "forbidden" : "error");
  return "error";
}

type AskBody = { data_source_id: number; question: string; existing_sql?: string; error?: string };

export type AskStreamFn<B> = (
  body: B,
  onEvent: (event: AskEvent) => void,
  signal?: AbortSignal,
) => Promise<void>;

export function useAskRun<B extends { data_source_id: number } = AskBody>(
  streamFn: AskStreamFn<B> = api.askStream as unknown as AskStreamFn<B>,
) {
  const streamFnRef = useRef(streamFn);
  streamFnRef.current = streamFn;
  const [state, dispatch] = useReducer(reduce, IDLE_RUN);
  const abortRef = useRef<AbortController | null>(null);
  const runIdRef = useRef(0);

  useEffect(() => () => abortRef.current?.abort(), []);

  const bodyRef = useRef<B | null>(null);

  const stream = useCallback((body: B) => {
    abortRef.current?.abort();
    const controller = new AbortController();
    abortRef.current = controller;
    streamFnRef
      .current(
        body,
        (event) => {
          if (!controller.signal.aborted) dispatch({ type: "event", event });
        },
        controller.signal,
      )
      .then(() => {
        if (!controller.signal.aborted) {
          dispatch({ type: "fail", message: "The AI stream ended unexpectedly. Try again." });
        }
      })
      .catch((err) => {
        if (err instanceof DOMException && err.name === "AbortError") return;
        dispatch({ type: "fail", message: askFailureMessage(err), reason: failureReason(err) });
      });
  }, []);

  const start = useCallback(
    (body: B, source: AskSource = "ask") => {
      bodyRef.current = body;
      dispatch({ type: "start", runId: ++runIdRef.current, source });
      stream(body);
    },
    [stream],
  );

  useEffect(() => {
    const body = bodyRef.current;
    if (!state.waiting || !body) return;
    let stopped = false;
    let timer: ReturnType<typeof setTimeout>;
    const cap = setTimeout(() => {
      stopped = true;
      clearTimeout(timer);
      dispatch({ type: "wait_failed", message: null });
    }, REFRESH_WAIT_CAP_MS);
    const poll = async () => {
      try {
        const res = await api.getDataSourceSchema(body.data_source_id, false);
        if (stopped) return;
        if (!res.refreshing) {
          stopped = true;
          clearTimeout(cap);
          if (res.refresh_error) {
            dispatch({ type: "wait_failed", message: REFRESH_FAILED_MESSAGE });
          } else {
            dispatch({ type: "retry" });
            stream(body);
          }
          return;
        }
      } catch {
        if (stopped) return;
      }
      timer = setTimeout(poll, POLL_MS);
    };
    void poll();
    return () => {
      stopped = true;
      clearTimeout(timer);
      clearTimeout(cap);
    };
  }, [state.waiting, state.runId, stream]);

  const cancel = useCallback(() => {
    abortRef.current?.abort();
    dispatch({ type: "cancel" });
  }, []);

  const reset = useCallback(() => {
    abortRef.current?.abort();
    dispatch({ type: "reset" });
  }, []);

  return { state, start, cancel, reset };
}

const ERROR_CAP = 200;

export function trimError(raw: string): { short: string; truncated: boolean } {
  const first = raw.split("\n").find((l) => l.trim() !== "")?.trim() ?? "";
  const short = first.length > ERROR_CAP ? `${first.slice(0, ERROR_CAP)}…` : first;
  return { short, truncated: short !== raw.trim() };
}

export function lastProblem(state: AskRunState): string | null {
  if (state.phase !== "done" || state.succeeded) return null;
  const lastRepair = state.steps.findLast((s) => s.kind === "repair");
  return (
    state.final?.error?.trim() ||
    (lastRepair?.kind === "repair" ? lastRepair.error.trim() : "") ||
    "the preview didn't run"
  );
}

export function previewLabel(p: { shown: number; rowCount: number; truncated: boolean }): string {
  return p.truncated ? `${p.shown} of ${p.rowCount}+ rows` : `${p.rowCount} ${p.rowCount === 1 ? "row" : "rows"}`;
}

