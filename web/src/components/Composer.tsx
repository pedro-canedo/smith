import { useState } from "react";
import { CornerDownLeft, Send, Square, Zap } from "lucide-react";
import { api } from "@/lib/api";
import { Button } from "@/components/ui/button";
import { Textarea } from "@/components/ui/input";
import { cn } from "@/lib/utils";

export function Composer({ busy }: { busy: boolean }) {
  const [text, setText] = useState("");

  const send = () => {
    const trimmed = text.trim();
    if (!trimmed) return;
    // Mirrors the TUI: a new prompt when idle, an interjection into the
    // running turn otherwise.
    void (busy ? api.interject(trimmed) : api.submit(trimmed));
    setText("");
  };

  return (
    <form
      className={cn(
        "panel flex items-end gap-2 p-2 transition-colors",
        busy && "border-ember/25",
      )}
      onSubmit={(event) => {
        event.preventDefault();
        send();
      }}
    >
      <Textarea
        value={text}
        rows={text.split("\n").length > 3 ? 5 : 2}
        onChange={(event) => setText(event.target.value)}
        // Enter sends, Shift+Enter breaks the line — the convention every
        // chat surface uses, and the one thing a textarea gets wrong by
        // default.
        onKeyDown={(event) => {
          if (event.key === "Enter" && !event.shiftKey) {
            event.preventDefault();
            send();
          }
        }}
        placeholder={
          busy ? "Interject into the running turn…" : "Ask smith anything…"
        }
        className="border-0 bg-transparent focus:bg-transparent"
      />
      <div className="flex shrink-0 items-center gap-1.5 pb-1">
        <span className="hidden items-center gap-1 pr-1 text-[0.625rem] text-disabled sm:flex">
          <CornerDownLeft className="size-3" />
          send
        </span>
        {busy && (
          <Button
            type="button"
            variant="danger"
            size="icon"
            title="stop the turn"
            onClick={() => void api.cancel()}
          >
            <Square />
          </Button>
        )}
        <Button
          type="submit"
          variant="primary"
          size="icon"
          disabled={text.trim().length === 0}
          title={busy ? "interject" : "send"}
        >
          {busy ? <Zap /> : <Send />}
        </Button>
      </div>
    </form>
  );
}
