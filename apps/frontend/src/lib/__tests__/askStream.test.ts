import { afterEach, describe, expect, it, vi } from "vitest";
import { api, ApiError } from "../api";
import type { AskEvent } from "@/types/api";

const body = { data_source_id: 1, question: "q" };

function sseResponse(chunks: string[]) {
  const enc = new TextEncoder();
  return new Response(
    new ReadableStream<Uint8Array>({
      start(c) {
        for (const ch of chunks) c.enqueue(enc.encode(ch));
        c.close();
      },
    }),
    { status: 200, headers: { "Content-Type": "text/event-stream" } },
  );
}

afterEach(() => vi.unstubAllGlobals());

describe("api.askStream", () => {
  it("emits typed events in order, skipping unknown/malformed ones", async () => {
    const fetchMock = vi.fn().mockResolvedValue(
      sseResponse([
        'event: status\ndata: {"text":"hi"}\n\nevent: draft\ndata: {"attempt":1,"sql":"select 1"}',
        '\n\nevent: nope\ndata: {}\n\nevent: repair\ndata: {bad\n\n',
        'event: failed\ndata: {"status":"rate_limited","message":"m","retry_after_secs":3}\n\n',
      ]),
    );
    vi.stubGlobal("fetch", fetchMock);
    const events: AskEvent[] = [];
    await api.askStream(body, (e) => events.push(e));
    expect(events).toEqual([
      { type: "status", text: "hi" },
      { type: "draft", attempt: 1, sql: "select 1" },
      { type: "failed", status: "rate_limited", message: "m", retry_after_secs: 3 },
    ]);
    const [url, init] = fetchMock.mock.calls[0];
    expect(url).toMatch(/\/api\/queries\/ask$/);
    expect(init).toMatchObject({ method: "POST", credentials: "include" });
    expect(JSON.parse(init.body)).toEqual(body);
  });

  it("throws ApiError on non-2xx", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue(
        new Response(JSON.stringify({ message: "nope" }), {
          status: 403,
          headers: { "Content-Type": "application/json" },
        }),
      ),
    );
    await expect(api.askStream(body, () => {})).rejects.toMatchObject({
      constructor: ApiError,
      status: 403,
      message: "nope",
    });
  });

  it("rejects when the signal aborts mid-stream", async () => {
    const ctrl = new AbortController();
    vi.stubGlobal(
      "fetch",
      vi.fn().mockImplementation(async (_u: string, init: RequestInit) => {
        const enc = new TextEncoder();
        return new Response(
          new ReadableStream<Uint8Array>({
            start(c) {
              c.enqueue(enc.encode('event: status\ndata: {"text":"a"}\n\n'));
              init.signal?.addEventListener("abort", () =>
                c.error(new DOMException("aborted", "AbortError")),
              );
            },
          }),
        );
      }),
    );
    const events: AskEvent[] = [];
    const p = api.askStream(body, (e) => {
      events.push(e);
      ctrl.abort();
    }, ctrl.signal);
    await expect(p).rejects.toMatchObject({ name: "AbortError" });
    expect(events).toHaveLength(1);
  });
});
