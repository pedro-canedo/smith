import { useEffect, useState } from "react";
import { ScrollText } from "lucide-react";
import type { SessionSummary } from "@/lib/types";
import { api, sessionId } from "@/lib/api";
import { Card } from "@/components/ui/card";
import { Badge } from "@/components/ui/badge";

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
    api
      .sessionMessages(open)
      .then((loaded) => setMessages(loaded as HistoryMessage[]))
      .catch((problem: Error) => setError(problem.message));
  }, [open]);

  if (error) return <p className="text-sm text-danger">{error}</p>;

  return (
    <div className="flex flex-col gap-3">
      {sessions.map((session) => (
        <div key={session.id}>
          <button
            className="flex w-full cursor-pointer items-center gap-2 rounded-md px-2 py-1.5 text-left text-sm hover:bg-hover"
            onClick={() => setOpen(open === session.id ? null : session.id)}
          >
            <ScrollText className="size-4 text-secondary" />
            <span className="flex-1">{session.title || session.id}</span>
            {session.id === sessionId && <Badge variant="ember">live</Badge>}
            <span className="text-xs text-disabled">
              {new Date(session.updated_at).toLocaleString()}
            </span>
          </button>
          {open === session.id && (
            <Card className="mt-1 flex max-h-96 flex-col gap-2 overflow-auto">
              {messages.map((message, index) => (
                <div key={index} className="text-sm">
                  <span className="text-xs font-semibold text-secondary">
                    {message.role}
                  </span>
                  {message.content.map((block, blockIndex) => (
                    <div key={blockIndex} className="whitespace-pre-wrap">
                      {block.type === "text"
                        ? block.text
                        : `[${block.type}${block.name ? `: ${block.name}` : ""}]`}
                    </div>
                  ))}
                </div>
              ))}
            </Card>
          )}
        </div>
      ))}
      {sessions.length === 0 && (
        <p className="text-sm text-disabled">No saved sessions in this project yet.</p>
      )}
    </div>
  );
}
