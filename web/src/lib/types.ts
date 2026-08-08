// Mirrors of the Rust wire types. The Rust side is the source of truth:
// AgentEvent's serde shape IS `--output-format stream-json`, and
// SessionProjection is defined in smith-cli/src/webconsole/state.rs.

export type TaskStatus =
  | "pending"
  | "in_progress"
  | "blocked"
  | "review"
  | "completed";

export interface Task {
  content: string;
  status: TaskStatus;
  id?: string;
  blocked_reason?: string;
  updated_at?: number;
}

export interface PermissionRequest {
  tool_call_id: string;
  tool_name: string;
  detail: string;
}

export interface UserQuestion {
  id: string;
  prompt: string;
  options: [string, string, string];
}

export type TranscriptItem =
  | { kind: "user"; text: string }
  | { kind: "assistant"; text: string }
  | { kind: "system"; text: string }
  | {
      kind: "tool_card";
      id: string;
      tool_name: string;
      input: unknown;
      progress: string[];
      output: string | null;
      is_error: boolean;
      running: boolean;
    };

export interface SessionProjection {
  seq: number;
  session_id: string;
  provider: string;
  model: string;
  phase: string;
  plan_gated: boolean;
  transcript: TranscriptItem[];
  streaming_text: string;
  tasks: Task[];
  pending_permission: PermissionRequest | null;
  pending_question: UserQuestion | null;
  usage: {
    input_tokens: number;
    output_tokens: number;
    cache_read: number;
    cache_write: number;
  };
  cost_usd: number;
  unpriced_turns: number;
  context: [number, number, boolean] | null;
  goal: string | null;
}

export type LinkGroup = "provider" | "service" | "reference";

/** One entry in the navigation rail's endpoint list. Resolved server-side
 * from the layered config — see smith-cli/src/webconsole/links.rs. */
export interface ConsoleLink {
  id: string;
  label: string;
  url: string;
  detail: string;
  group: LinkGroup;
  /** The URL leaves this machine, so the anchor gets rel="noreferrer". */
  external: boolean;
  /** The provider serving this session. */
  active: boolean;
}

/** What `/api/meta` answers: the half of the session that never changes. */
export interface ConsoleMeta {
  session_id: string;
  provider: string;
  model: string;
  version: string;
  cwd: string;
  started_at_ms: number;
  links: ConsoleLink[];
}

export interface SessionSummary {
  id: string;
  title: string;
  updated_at: number;
}

/** One stream-json line: `{"type": ..., "data": ...}`. */
export interface WireEvent {
  type: string;
  data?: unknown;
}
