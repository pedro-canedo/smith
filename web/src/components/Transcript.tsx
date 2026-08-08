import { useEffect, useRef, useState } from "react";
import {
  ChevronRight,
  CircleCheck,
  CircleX,
  Loader2,
  MessageSquareDashed,
  Terminal,
  User,
} from "lucide-react";
import type { SessionProjection, TranscriptItem } from "@/lib/types";
import { Badge } from "@/components/ui/badge";
import { cn } from "@/lib/utils";

/** The first line of a tool's arguments, which is what identifies the call —
 * a path, a command, a query. Rendering the whole JSON turns a transcript
 * into a wall; the card expands for the rest. */
function summarise(input: unknown): string | null {
  if (input === null || typeof input !== "object") return null;
  const entries = Object.entries(input as Record<string, unknown>);
  const first = entries.find(([, value]) => typeof value === "string");
  if (!first) return entries.length > 0 ? `${entries.length} arguments` : null;
  const text = String(first[1]).split("\n")[0] ?? "";
  return text.length > 120 ? `${text.slice(0, 120)}…` : text;
}

function ToolCard({ item }: { item: Extract<TranscriptItem, { kind: "tool_card" }> }) {
  const [open, setOpen] = useState(false);
  const summary = summarise(item.input);
  const tone = item.running ? "text-ember" : item.is_error ? "text-danger" : "text-success";

  return (
    <div
      className={cn(
        "panel overflow-hidden",
        item.running && "border-ember/25",
        item.is_error && "border-danger/25",
      )}
    >
      <button
        onClick={() => setOpen((previous) => !previous)}
        className="flex w-full cursor-pointer items-center gap-2.5 px-3.5 py-2.5 text-left hover:bg-hover/40"
      >
        <span className={tone}>
          {item.running ? (
            <Loader2 className="size-4 animate-spin" />
          ) : item.is_error ? (
            <CircleX className="size-4" />
          ) : (
            <CircleCheck className="size-4" />
          )}
        </span>
        <span className="font-mono text-sm text-text">{item.tool_name}</span>
        {summary && (
          <span className="min-w-0 flex-1 truncate font-mono text-xs text-secondary">
            {summary}
          </span>
        )}
        <span className="ml-auto flex items-center gap-2">
          {item.running && <Badge variant="ember">running</Badge>}
          {item.is_error && <Badge variant="danger">error</Badge>}
          <ChevronRight
            className={cn(
              "size-3.5 text-disabled transition-transform",
              open && "rotate-90",
            )}
          />
        </span>
      </button>

      {item.progress.length > 0 && !open && (
        <div className="truncate border-t border-text/6 px-3.5 py-1.5 pl-10 font-mono text-xs text-secondary">
          {item.progress[item.progress.length - 1]}
        </div>
      )}

      {open && (
        <div className="flex flex-col gap-2 border-t border-text/6 px-3.5 py-2.5">
          <pre className="overflow-x-auto font-mono text-xs whitespace-pre-wrap text-secondary">
            {JSON.stringify(item.input, null, 2)}
          </pre>
          {item.progress.length > 0 && (
            <div className="flex flex-col gap-0.5 border-l-2 border-text/10 pl-2.5">
              {item.progress.map((line, index) => (
                <span key={index} className="font-mono text-xs text-secondary">
                  {line}
                </span>
              ))}
            </div>
          )}
          {item.output && (
            <pre
              className={cn(
                "max-h-72 overflow-auto rounded-lg bg-base/60 p-2.5 font-mono text-xs whitespace-pre-wrap",
                item.is_error ? "text-danger" : "text-secondary",
              )}
            >
              {item.output}
            </pre>
          )}
        </div>
      )}

      {/* Errors stay visible collapsed: the tail is what says why. */}
      {item.is_error && item.output && !open && (
        <pre className="max-h-20 overflow-hidden border-t border-danger/15 px-3.5 py-1.5 pl-10 font-mono text-xs whitespace-pre-wrap text-danger">
          {item.output.split("\n").slice(-3).join("\n")}
        </pre>
      )}
    </div>
  );
}

function Item({ item }: { item: TranscriptItem }) {
  switch (item.kind) {
    case "user":
      return (
        <div className="flex justify-end">
          <div className="flex max-w-[85%] items-start gap-2.5 rounded-panel rounded-br-sm border border-ember/25 bg-ember/8 px-4 py-2.5">
            <div className="text-sm whitespace-pre-wrap text-text">{item.text}</div>
            <User className="mt-0.5 size-3.5 shrink-0 text-ember" />
          </div>
        </div>
      );
    case "assistant":
      return (
        <div className="text-sm leading-relaxed whitespace-pre-wrap text-text">
          {item.text}
        </div>
      );
    case "system":
      return (
        <div className="flex items-center gap-1.5 font-mono text-xs text-disabled">
          <Terminal className="size-3 shrink-0" />
          <span className="min-w-0 truncate">{item.text}</span>
        </div>
      );
    case "tool_card":
      return <ToolCard item={item} />;
  }
}

export function Transcript({ state }: { state: SessionProjection }) {
  const bottom = useRef<HTMLDivElement>(null);
  useEffect(() => {
    bottom.current?.scrollIntoView({ block: "end" });
  }, [state.transcript.length, state.streaming_text]);

  if (state.transcript.length === 0 && !state.streaming_text) {
    return (
      <div className="flex flex-col items-center gap-2 py-16 text-center">
        <MessageSquareDashed className="size-6 text-disabled" />
        <p className="text-sm text-secondary">Nothing said yet.</p>
        <p className="max-w-sm text-xs text-disabled">
          Type below, or keep working in the terminal — both frontends drive the
          same session and see each other's messages.
        </p>
      </div>
    );
  }

  return (
    <div className="flex flex-col gap-4">
      {state.transcript.map((item, index) => (
        <Item key={index} item={item} />
      ))}
      {state.streaming_text && (
        <div className="text-sm leading-relaxed whitespace-pre-wrap text-text">
          {state.streaming_text}
          <span className="ml-0.5 inline-block h-3.5 w-1.5 translate-y-0.5 bg-ember breathe" />
        </div>
      )}
      <div ref={bottom} />
    </div>
  );
}
