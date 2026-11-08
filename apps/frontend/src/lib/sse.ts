const MAX_PENDING_BYTES = 1024 * 1024;

export interface SseMessage {
  event: string;
  data: string;
}

/** Parses an SSE byte stream into messages. Decodes with `stream: true` so a multibyte char split
 * across chunks survives; a trailing event without a blank line is still dispatched at EOF. */
export async function readSse(
  body: ReadableStream<Uint8Array>,
  onMessage: (msg: SseMessage) => void,
): Promise<void> {
  const reader = body.getReader();
  const decoder = new TextDecoder();
  let buffer = "";
  let event = "";
  let data: string[] = [];

  const dispatch = () => {
    if (data.length > 0) onMessage({ event: event || "message", data: data.join("\n") });
    event = "";
    data = [];
  };

  const handleLine = (line: string) => {
    if (line === "") return dispatch();
    if (line.startsWith(":")) return;
    const i = line.indexOf(":");
    const field = i === -1 ? line : line.slice(0, i);
    let value = i === -1 ? "" : line.slice(i + 1);
    if (value.startsWith(" ")) value = value.slice(1);
    if (field === "event") event = value;
    else if (field === "data") data.push(value);
  };

  // A trailing "\r" is held back: the next chunk may start with the "\n" of a CRLF.
  const drain = (final: boolean) => {
    const held = !final && buffer.endsWith("\r") ? "\r" : "";
    const lines = buffer.slice(0, buffer.length - held.length).split(/\r\n|\n|\r/);
    buffer = final ? "" : (lines.pop() ?? "") + held;
    for (const l of lines) handleLine(l);
  };

  try {
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      buffer += decoder.decode(value, { stream: true });
      drain(false);
      if (buffer.length > MAX_PENDING_BYTES) throw new Error("SSE line exceeds the 1 MB limit");
    }
    buffer += decoder.decode();
    drain(true);
    dispatch();
  } finally {
    reader.releaseLock();
  }
}
