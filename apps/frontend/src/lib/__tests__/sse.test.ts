import { describe, expect, it } from "vitest";
import { readSse, type SseMessage } from "../sse";

function streamOf(chunks: (string | Uint8Array)[]) {
  const enc = new TextEncoder();
  return new ReadableStream<Uint8Array>({
    start(c) {
      for (const ch of chunks) c.enqueue(typeof ch === "string" ? enc.encode(ch) : ch);
      c.close();
    },
  });
}

async function parse(chunks: (string | Uint8Array)[]) {
  const out: SseMessage[] = [];
  await readSse(streamOf(chunks), (m) => out.push(m));
  return out;
}

describe("readSse", () => {
  it("dispatches event/data on blank line", async () => {
    expect(await parse(["event: a\ndata: {\"x\":1}\n\nevent: b\ndata: 2\n\n"])).toEqual([
      { event: "a", data: '{"x":1}' },
      { event: "b", data: "2" },
    ]);
  });

  it("joins multi-line data and defaults event to message", async () => {
    expect(await parse(["data: one\ndata: two\n\n"])).toEqual([
      { event: "message", data: "one\ntwo" },
    ]);
  });

  it("ignores comments and keep-alives", async () => {
    expect(await parse([": ping\n\nevent: a\n: note\ndata: 1\n\n"])).toEqual([
      { event: "a", data: "1" },
    ]);
  });

  it("handles a split inside a line and inside a terminator", async () => {
    expect(await parse(["eve", "nt: a\nda", "ta: 1\n", "\n", "event: b\ndata: 2\n\n"])).toEqual([
      { event: "a", data: "1" },
      { event: "b", data: "2" },
    ]);
  });

  it("handles CRLF, including a chunk split between \\r and \\n", async () => {
    expect(await parse(["event: a\r\ndata: 1\r", "\n\r", "\nevent: b\r\ndata: 2\r\n\r\n"])).toEqual([
      { event: "a", data: "1" },
      { event: "b", data: "2" },
    ]);
  });

  it("survives a split inside a multibyte UTF-8 char", async () => {
    const bytes = new TextEncoder().encode("event: a\ndata: héllo €\n\n");
    const cut = bytes.indexOf(0xc3) + 1;
    const euro = bytes.indexOf(0xe2) + 1;
    expect(
      await parse([bytes.slice(0, cut), bytes.slice(cut, euro), bytes.slice(euro)]),
    ).toEqual([{ event: "a", data: "héllo €" }]);
  });

  it("dispatches a trailing event with no final blank line", async () => {
    expect(await parse(["event: a\ndata: 1\n\nevent: b\ndata: 2"])).toEqual([
      { event: "a", data: "1" },
      { event: "b", data: "2" },
    ]);
  });

  it("drops events with no data", async () => {
    expect(await parse(["event: a\n\n"])).toEqual([]);
  });

  it("throws when a line without a newline exceeds the pending-buffer cap", async () => {
    const big = "data: " + "x".repeat(1024 * 1024 + 1);
    await expect(parse([big])).rejects.toThrow(/1 MB/);
  });
});
