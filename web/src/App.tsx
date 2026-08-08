import { useEffect, useRef, useState } from "react";
import { Flame } from "lucide-react";
import type { ConsoleMeta, SessionProjection } from "@/lib/types";
import { api, openEvents } from "@/lib/api";
import { Rail, type View } from "@/components/layout/Rail";
import { TopBar } from "@/components/layout/TopBar";
import { StatsPanel } from "@/components/layout/StatsPanel";
import { Transcript } from "@/components/Transcript";
import { Composer } from "@/components/Composer";
import { PermissionPrompt, QuestionPrompt } from "@/components/Approvals";
import { Board } from "@/components/Board";
import { History } from "@/components/History";
import { useMediaQuery, usePreference } from "@/lib/hooks";

export default function App() {
  const [state, setState] = useState<SessionProjection | null>(null);
  const [meta, setMeta] = useState<ConsoleMeta | null>(null);
  const [view, setView] = useState<View>("session");
  const [down, setDown] = useState(false);
  const [railCollapsed, toggleRail] = usePreference("smith.rail.collapsed", false);
  const [statsOpen, toggleStats] = usePreference("smith.stats.open", true);
  // `xl` — the same breakpoint the panel's own class uses.
  const wideEnough = useMediaQuery("(min-width: 80rem)");
  const statsVisible = statsOpen && wideEnough;
  // Events that arrive while a resnapshot is in flight are already covered
  // by it — the seq guard drops the stale ones.
  const seqRef = useRef(0);

  useEffect(() => {
    // Constants, fetched once: nothing in /api/meta changes while the
    // session runs, so it is deliberately not part of the resync loop.
    void api.meta().then(setMeta).catch(() => undefined);
  }, []);

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

  if (state === null) {
    return (
      <main className="grid h-dvh place-items-center">
        <div className="flex flex-col items-center gap-3 text-sm text-secondary">
          <span className="grid size-11 place-items-center rounded-xl bg-ember/12 ring-1 ring-ember/25">
            <Flame className={down ? "size-5 text-danger" : "size-5 text-ember breathe"} />
          </span>
          {down
            ? "session ended — restart smith and reload"
            : "connecting to the session…"}
        </div>
      </main>
    );
  }

  const busy = state.phase !== "idle";

  return (
    <div className="flex h-dvh overflow-hidden">
      <Rail
        view={view}
        onView={setView}
        state={state}
        meta={meta}
        collapsed={railCollapsed}
        onToggle={toggleRail}
        connected={!down}
      />

      <div className="flex min-w-0 flex-1 flex-col">
        <TopBar
          view={view}
          state={state}
          down={down}
          statsOpen={statsVisible}
          onToggleStats={toggleStats}
        />

        {/* Asks sit above the scroll region, never inside it: an approval
            that scrolls out of view is an approval nobody answers. */}
        {(state.pending_permission || state.pending_question) && (
          <div className="flex shrink-0 flex-col gap-2 px-5 pt-4">
            {state.pending_permission && (
              <PermissionPrompt request={state.pending_permission} />
            )}
            {state.pending_question && (
              <QuestionPrompt question={state.pending_question} />
            )}
          </div>
        )}

        <main className="min-h-0 flex-1 overflow-y-auto px-5 py-4">
          <div className="mx-auto w-full max-w-4xl">
            {view === "session" && <Transcript state={state} />}
            {view === "board" && <Board tasks={state.tasks} />}
            {view === "history" && <History />}
          </div>
        </main>

        {view === "session" && (
          <div className="shrink-0 border-t border-text/8 px-5 py-3">
            <div className="mx-auto w-full max-w-4xl">
              <Composer busy={busy} />
            </div>
          </div>
        )}
      </div>

      {statsVisible && <StatsPanel state={state} meta={meta} />}
    </div>
  );
}
