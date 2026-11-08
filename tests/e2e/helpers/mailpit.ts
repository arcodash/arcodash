export const MAILPIT_URL = "http://localhost:8026";

export interface MailpitMessage {
  ID: string;
  Subject: string;
  Snippet: string;
  To: { Address: string }[];
}

export async function mailpitMessages(): Promise<MailpitMessage[]> {
  const res = await fetch(`${MAILPIT_URL}/api/v1/messages?limit=100`);
  if (!res.ok) return [];
  return ((await res.json()) as { messages: MailpitMessage[] }).messages ?? [];
}

export async function pollMailpitFor(
  predicate: (m: MailpitMessage) => boolean,
  timeoutMs: number,
): Promise<MailpitMessage> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const found = (await mailpitMessages()).find(predicate);
    if (found) return found;
    await new Promise((r) => setTimeout(r, 2000));
  }
  throw new Error(`No matching mailpit message within ${timeoutMs}ms`);
}

export function pollMailpitForSubject(substring: string, timeoutMs: number): Promise<MailpitMessage> {
  return pollMailpitFor((m) => m.Subject?.includes(substring), timeoutMs);
}

/** A message's full content (Subject + both MIME body parts). */
export async function fetchMailpitMessage(id: string): Promise<{ Subject: string; Text: string; HTML: string }> {
  const res = await fetch(`${MAILPIT_URL}/api/v1/message/${id}`);
  if (!res.ok) throw new Error(`Mailpit message fetch failed: ${res.status}`);
  return res.json();
}
