import { useEffect, useRef } from "react";
import {
  CircleCheck,
  CircleX,
  Loader2,
  TerminalSquare,
} from "lucide-react";
import type { SessionProjection, TranscriptItem } from "@/lib/types";
import { Card } from "@/components/ui/card";
import { Badge } from "@/components/ui/badge";

function ToolCard({ item }: { item: Extract<TranscriptItem, { kind: "tool_card" }> }) {
  return (
    <Card className="bg-raised py-2">
      <div className="flex items-center gap-2 text-sm">
        {item.running ? (
          <Loader2 className="size-4 animate-spin text-ember" />
        ) : item.is_error ? (
          <CircleX className="size-4 text-danger" />
        ) : (
          <CircleCheck className="size-4 text-success" />
        )}
        <span className="text-text">{item.tool_name}</span>
        <Badge variant={item.running ? "ember" : item.is_error ? "danger" : "default"}>
          {item.running ? "running" : item.is_error ? "error" : "done"}
        </Badge>
      </div>
      {item.progress.length > 0 && (
        <div className="mt-1 pl-6 text-xs text-secondary">
          {item.progress[item.progress.length - 1]}
        </div>
      )}
      {item.is_error && item.output && (
        <pre className="mt-1 max-h-24 overflow-auto pl-6 text-xs text-danger whitespace-pre-wrap">
          {item.output.split("\n").slice(-3).join("\n")}
        </pre>
      )}
    </Card>
  );
}

function Item({ item }: { item: TranscriptItem }) {
  switch (item.kind) {
    case "user":
      return (
        <Card className="border border-ember/40">
          <div className="mb-1 text-xs font-semibold text-ember">You</div>
          <div className="whitespace-pre-wrap text-sm">{item.text}</div>
        </Card>
      );
    case "assistant":
      return (
        <div className="border-l-2 border-ember pl-3 whitespace-pre-wrap text-sm">
          {item.text}
        </div>
      );
    case "system":
      return (
        <div className="flex items-center gap-1.5 text-xs text-disabled">
          <TerminalSquare className="size-3" /> {item.text}
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

  return (
    <div className="flex flex-col gap-3">
      {state.transcript.map((item, index) => (
        <Item key={index} item={item} />
      ))}
      {state.streaming_text && (
        <div className="border-l-2 border-ember pl-3 whitespace-pre-wrap text-sm">
          {state.streaming_text}
          <span className="animate-pulse text-ember">▌</span>
        </div>
      )}
      <div ref={bottom} />
    </div>
  );
}
