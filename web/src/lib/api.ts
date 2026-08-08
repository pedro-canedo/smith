// The console's half of the wire protocol (docs/web-console.md §2): the
// token rides X-Smith-Token on every call except the SSE stream, where the
// EventSource API cannot set headers and ?t= is the sanctioned exception.

const params = new URLSearchParams(window.location.search);
export const token: string =
  params.get("t") ?? import.meta.env.VITE_SMITH_TOKEN ?? "";

export const sessionId: string =
  window.location.pathname.match(/^\/s\/([A-Za-z0-9-]+)/)?.[1] ?? "";

async function request<T>(path: string, body?: unknown): Promise<T> {
  const response = await fetch(path, {
    method: body === undefined ? "GET" : "POST",
    headers:
      body === undefined
        ? { "X-Smith-Token": token }
        : { "X-Smith-Token": token, "Content-Type": "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  if (!response.ok) {
    const text = await response.text();
    throw new Error(`${response.status}: ${text}`);
  }
  return (await response.json()) as T;
}

export const api = {
  state: () => request<import("./types").SessionProjection>("/api/state"),
  // Fetched once. Nothing in it moves, and /api/state is refetched per event.
  meta: () => request<import("./types").ConsoleMeta>("/api/meta"),
  sessions: () => request<import("./types").SessionSummary[]>("/api/sessions"),
  sessionMessages: (id: string) =>
    request<unknown[]>(`/api/sessions/${encodeURIComponent(id)}/messages`),
  submit: (text: string) =>
    request("/api/action", { type: "submit_message", data: text }),
  interject: (text: string) =>
    request("/api/action", { type: "interject", data: text }),
  cancel: () => request("/api/action", { type: "cancel_generation" }),
  answerPermission: (tool_call_id: string, decision: string) =>
    request("/api/ask/answer", { kind: "permission", tool_call_id, decision }),
  answerQuestion: (id: string, answer: string) =>
    request("/api/ask/answer", { kind: "question", id, answer }),
};

/** Opens the SSE stream. `onEvent` gets every framed event newer than the
 * snapshot; `onResync` fires whenever the client must refetch `/api/state`
 * (open, reconnect, or a `gap` frame after broadcast lag). */
export function openEvents(handlers: {
  onEvent: (event: import("./types").WireEvent, seq: number) => void;
  onResync: () => void;
  onDown: () => void;
}): () => void {
  const source = new EventSource(`/api/events?t=${encodeURIComponent(token)}`);
  source.onopen = handlers.onResync;
  source.onmessage = (message) => {
    const event = JSON.parse(message.data) as import("./types").WireEvent;
    if (event.type === "gap") {
      handlers.onResync();
      return;
    }
    if (event.type === "hello") return;
    handlers.onEvent(event, Number(message.lastEventId ?? 0));
  };
  source.onerror = handlers.onDown;
  return () => source.close();
}
