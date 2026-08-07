import { useEffect, useMemo, useRef, useState } from "react";
import { Flame, KanbanSquare, MessageSquare, ScrollText, WifiOff } from "lucide-react";
import type { SessionProjection } from "@/lib/types";
import { api, openEvents } from "@/lib/api";
import { Badge } from "@/components/ui/badge";
import { Transcript } from "@/components/Transcript";
import { Composer } from "@/components/Composer";
import { PermissionPrompt, QuestionPrompt } from "@/components/Approvals";
import { Board } from "@/components/Board";
import { History } from "@/components/History";
import { cn } from "@/lib/utils";

type Tab = "session" | "board" | "history";

export default function App() {
  const [state, setState] = useState<SessionProjection | null>(null);
  const [tab, setTab] = useState<Tab>("session");
  const [down, setDown] = useState(false);
  // Events that arrive while a resnapshot is in flight are already covered
  // by it — the seq guard drops the stale ones.
  const seqRef = useRef(0);

  useEffect(() => {
    const resync = () => {
      void api
        .state()
        .then((snapshot) => {
          seqRef.current = snapshot.seq;
          setState(snapshot);
          setDown(false);
        })
        .catch(() => setDown(true));
    };
    // The P0 client re-snapshots on every event rather than replaying the
    // 26-variant reducer client-side: the projection server-side is already
    // exactly that reducer, /api/state is loopback-cheap, and two sources of
    // truth for one transcript is the bug factory this avoids. The seq guard
    // keeps it monotonic; EventSource gives reconnection.
    const close = openEvents({
      onEvent: (_event, seq) => {
        if (seq <= seqRef.current) return;
        resync();
      },
      onResync: resync,
      onDown: () => setDown(true),
    });
    return close;
  }, []);

  const busy = state !== null && state.phase !== "idle";
  const contextLabel = useMemo(() => {
    if (!state?.context) return null;
    const [used, window, estimated] = state.context;
    const percent = Math.round((used / Math.max(window, 1)) * 100);
    return `${estimated ? "~" : ""}${percent}% ${Math.round(used / 1000)}k/${Math.round(window / 1000)}k`;
  }, [state]);

  if (state === null) {
    return (
      <main className="mx-auto max-w-4xl p-6 text-sm text-secondary">
        {down ? "session ended — restart smith --web and reload" : "connecting…"}
      </main>
    );
  }

  const tabs: { id: Tab; title: string; icon: typeof MessageSquare }[] = [
    { id: "session", title: "Session", icon: MessageSquare },
    { id: "board", title: "Board", icon: KanbanSquare },
    { id: "history", title: "History", icon: ScrollText },
  ];

  return (
    <main className="mx-auto flex h-dvh max-w-4xl flex-col gap-3 p-4">
      <header className="flex flex-wrap items-center gap-2">
        <Flame className="size-5 text-ember" />
        <span className="font-semibold text-ember">smith</span>
        <span className="text-xs text-secondary">
          {state.provider} · {state.model}
        </span>
        <Badge variant={busy ? "ember" : "default"}>{state.phase}</Badge>
        {state.plan_gated && <Badge variant="plan">PLAN MODE</Badge>}
        {contextLabel && <Badge>{contextLabel}</Badge>}
        {state.cost_usd > 0 && (
          <Badge variant="amber">~${state.cost_usd.toFixed(4)}</Badge>
        )}
        {state.unpriced_turns > 0 && (
          <Badge variant="warning">+{state.unpriced_turns} unpriced</Badge>
        )}
        {down && (
          <Badge variant="danger">
            <WifiOff className="size-3" /> reconnecting
          </Badge>
        )}
        <nav className="ml-auto flex gap-1">
          {tabs.map(({ id, title, icon: Icon }) => (
            <button
              key={id}
              onClick={() => setTab(id)}
              className={cn(
                "flex cursor-pointer items-center gap-1.5 rounded-md px-2.5 py-1 text-sm",
                tab === id ? "bg-overlay text-ember" : "text-secondary hover:bg-hover",
              )}
            >
              <Icon className="size-4" />
              {title}
            </button>
          ))}
        </nav>
      </header>

      {state.goal && (
        <div className="text-xs text-secondary">
          goal: <span className="text-text">{state.goal}</span>
        </div>
      )}

      {state.pending_permission && (
        <PermissionPrompt request={state.pending_permission} />
      )}
      {state.pending_question && <QuestionPrompt question={state.pending_question} />}

      <section className="min-h-0 flex-1 overflow-y-auto pr-1">
        {tab === "session" && <Transcript state={state} />}
        {tab === "board" && <Board tasks={state.tasks} />}
        {tab === "history" && <History />}
      </section>

      {tab === "session" && <Composer busy={busy} />}
    </main>
  );
}
