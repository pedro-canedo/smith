import { useState } from "react";
import { Send, Square } from "lucide-react";
import { api } from "@/lib/api";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";

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
      className="flex gap-2"
      onSubmit={(event) => {
        event.preventDefault();
        send();
      }}
    >
      <Input
        value={text}
        onChange={(event) => setText(event.target.value)}
        placeholder={busy ? "Interject into the running turn…" : "Ask anything…"}
        autoComplete="off"
      />
      <Button type="submit" title={busy ? "interject" : "send"}>
        <Send className="size-4" />
      </Button>
      {busy && (
        <Button
          type="button"
          variant="danger"
          title="cancel the turn"
          onClick={() => void api.cancel()}
        >
          <Square className="size-4" />
        </Button>
      )}
    </form>
  );
}
