import { PanelRight, Target, WifiOff } from "lucide-react";
import type { SessionProjection } from "@/lib/types";
import type { View } from "./Rail";
import { compact, money } from "@/lib/format";
import { Badge, Dot } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { occupancyTone } from "@/components/ui/meter";
import { cn } from "@/lib/utils";

const TITLES: Record<View, string> = {
  session: "Session",
  board: "Task board",
  history: "History",
};

export function TopBar({
  view,
  state,
  down,
  statsOpen,
  onToggleStats,
}: {
  view: View;
  state: SessionProjection;
  down: boolean;
  statsOpen: boolean;
  onToggleStats: () => void;
}) {
  const busy = state.phase !== "idle";
  const [used, window_] = state.context ?? [0, 0];
  const fraction = window_ > 0 ? used / window_ : 0;

  return (
    <header className="flex shrink-0 items-center gap-3 border-b border-text/8 px-5 py-3">
      <div className="min-w-0">
        <h1 className="text-sm font-semibold tracking-tight text-text">
          {TITLES[view]}
        </h1>
        <div className="flex items-center gap-1.5 text-[0.6875rem] text-disabled">
          <Dot className={busy ? "text-ember" : "text-disabled"} pulse={busy} />
          <span>{state.phase}</span>
          <span className="text-text/15">·</span>
          <span className="truncate font-mono">{state.model}</span>
        </div>
      </div>

      {state.goal && (
        <div
          className="hidden min-w-0 items-center gap-1.5 rounded-lg border border-text/8 bg-overlay/50 px-2.5 py-1 text-xs text-secondary lg:flex"
          title={state.goal}
        >
          <Target className="size-3.5 shrink-0 text-ember" />
          <span className="max-w-72 truncate">{state.goal}</span>
        </div>
      )}

      <div className="ml-auto flex items-center gap-2">
        {state.plan_gated && <Badge variant="plan">plan mode</Badge>}
        {down && (
          <Badge variant="danger">
            <WifiOff /> reconnecting
          </Badge>
        )}
        {/* Mirrors of the stats panel, for the widths where it is hidden. */}
        {!statsOpen && state.context && (
          <Badge variant="outline" className={cn("font-mono", occupancyTone(fraction))}>
            {Math.round(fraction * 100)}% · {compact(used)}
          </Badge>
        )}
        {!statsOpen && state.cost_usd > 0 && (
          <Badge variant="outline" className="font-mono">
            {money(state.cost_usd)}
          </Badge>
        )}
        <Button
          variant="ghost"
          size="icon-sm"
          onClick={onToggleStats}
          title={statsOpen ? "hide statistics" : "show statistics"}
          aria-pressed={statsOpen}
          className={cn(statsOpen && "text-ember")}
        >
          <PanelRight />
        </Button>
      </div>
    </header>
  );
}
