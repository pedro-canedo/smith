import { useEffect, useState } from "react";
import { ChevronRight, ScrollText } from "lucide-react";
import type { SessionSummary } from "@/lib/types";
import { api, sessionId } from "@/lib/api";
import { ago } from "@/lib/format";
import { Badge } from "@/components/ui/badge";
import { cn } from "@/lib/utils";

interface HistoryBlock {
  type: string;
  text?: string;
  name?: string;
}

interface HistoryMessage {
  role: string;
  content: HistoryBlock[];
}

export function History() {
  const [sessions, setSessions] = useState<SessionSummary[]>([]);
  const [open, setOpen] = useState<string | null>(null);
  const [messages, setMessages] = useState<HistoryMessage[]>([]);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    api
      .sessions()
      .then(setSessions)
      .catch((problem: Error) => setError(problem.message));
  }, []);

  useEffect(() => {
    if (!open) return;
    setMessages([]);
    api
      .sessionMessages(open)
      .then((loaded) => setMessages(loaded as HistoryMessage[]))
      .catch((problem: Error) => setError(problem.message));
  }, [open]);

  if (error) {
    return (
      <div className="panel border-danger/30 px-4 py-3 text-sm text-danger">{error}</div>
    );
  }

  if (sessions.length === 0) {
    return (
      <div className="flex flex-col items-center gap-2 py-16 text-center">
        <ScrollText className="size-6 text-disabled" />
        <p className="text-sm text-secondary">No saved sessions in this project.</p>
      </div>
    );
  }

  return (
    <div className="flex flex-col gap-1.5">
      {sessions.map((session) => {
        const isOpen = open === session.id;
        return (
          <div key={session.id} className={cn("panel overflow-hidden", !isOpen && "border-transparent bg-transparent backdrop-blur-none")}>
            <button
              className="flex w-full cursor-pointer items-center gap-2.5 px-3 py-2 text-left hover:bg-hover/50"
              onClick={() => setOpen(isOpen ? null : session.id)}
            >
              <ChevronRight
                className={cn(
                  "size-3.5 shrink-0 text-disabled transition-transform",
                  isOpen && "rotate-90",
                )}
              />
              <span className="min-w-0 flex-1 truncate text-sm">
                {session.title || session.id}
              </span>
              {session.id === sessionId && <Badge variant="ember">live</Badge>}
              <span
                className="shrink-0 font-mono text-[0.625rem] text-disabled"
                title={new Date(session.updated_at).toLocaleString()}
              >
                {ago(session.updated_at)}
              </span>
            </button>

            {isOpen && (
              <div className="flex max-h-[28rem] flex-col gap-3 overflow-auto border-t border-text/8 px-4 py-3">
                {messages.length === 0 && (
                  <p className="text-xs text-disabled">loading…</p>
                )}
                {messages.map((message, index) => (
                  <div key={index} className="flex flex-col gap-1">
                    <span className="eyebrow">{message.role}</span>
                    {message.content.map((block, blockIndex) => (
                      <div
                        key={blockIndex}
                        className={cn(
                          "text-sm whitespace-pre-wrap",
                          block.type !== "text" &&
                            "font-mono text-xs text-disabled",
                        )}
                      >
                        {block.type === "text"
                          ? block.text
                          : `[${block.type}${block.name ? `: ${block.name}` : ""}]`}
                      </div>
                    ))}
                  </div>
                ))}
              </div>
            )}
          </div>
        );
      })}
    </div>
  );
}
