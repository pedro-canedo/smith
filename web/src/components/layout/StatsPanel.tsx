import { useEffect, useState } from "react";
import {
  ArrowDownToLine,
  ArrowUpFromLine,
  CircleCheck,
  CircleDashed,
  DatabaseZap,
  Eye,
  Gauge,
  Loader2,
  OctagonX,
  Wrench,
} from "lucide-react";
import type { ConsoleMeta, SessionProjection, TaskStatus } from "@/lib/types";
import { compact, duration, money } from "@/lib/format";
import { Section, Separator, Stat } from "@/components/ui/card";
import { Badge, Dot } from "@/components/ui/badge";
import { Meter, Ring, occupancyTone } from "@/components/ui/meter";
import { cn } from "@/lib/utils";

const BOARD_ROWS: { status: TaskStatus; label: string; icon: typeof Eye; tone: string }[] = [
  { status: "in_progress", label: "Doing", icon: Loader2, tone: "text-ember" },
  { status: "blocked", label: "Blocked", icon: OctagonX, tone: "text-danger" },
  { status: "review", label: "Review", icon: Eye, tone: "text-amber" },
  { status: "pending", label: "Backlog", icon: CircleDashed, tone: "text-secondary" },
  { status: "completed", label: "Done", icon: CircleCheck, tone: "text-success" },
];

/** Uptime has to keep moving on its own — no event arrives to say a second
 * passed, and a stale "0s" on an hour-old session is worse than no number. */
function useNow(intervalMs = 1000): number {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const id = window.setInterval(() => setNow(Date.now()), intervalMs);
    return () => window.clearInterval(id);
  }, [intervalMs]);
  return now;
}

export function StatsPanel({
  state,
  meta,
  className,
}: {
  state: SessionProjection;
  meta: ConsoleMeta | null;
  className?: string;
}) {
  const now = useNow();

  const [used, window_, estimated] = state.context ?? [0, 0, false];
  const fraction = window_ > 0 ? used / window_ : 0;
  const tone = occupancyTone(fraction);

  const { input_tokens, output_tokens, cache_read, cache_write } = state.usage;
  const totalTokens = input_tokens + output_tokens + cache_read + cache_write;
  // Of everything that was read into the model this session, how much came
  // from cache. A high number is the difference between a cheap long session
  // and an expensive one, and it is invisible in the cost line alone.
  const cacheable = input_tokens + cache_read;
  const cacheHit = cacheable > 0 ? Math.round((cache_read / cacheable) * 100) : null;

  const toolCalls = state.transcript.filter((item) => item.kind === "tool_card").length;
  const running = state.transcript.find(
    (item) => item.kind === "tool_card" && item.running,
  );

  return (
    <aside
      className={cn(
        "flex h-full w-72 shrink-0 flex-col gap-4 overflow-y-auto border-l border-text/8",
        "bg-raised/40 px-4 py-4",
        className,
      )}
    >
      <Section
        title="Context"
        action={
          estimated ? (
            <Badge variant="outline" title="token counts are estimated locally">
              est.
            </Badge>
          ) : undefined
        }
      >
        {state.context ? (
          <div className="flex items-center gap-4">
            <Ring
              fraction={fraction}
              tone={tone}
              label={`${Math.round(fraction * 100)}%`}
              caption={`${compact(used)}/${compact(window_)}`}
            />
            <div className="flex min-w-0 flex-1 flex-col gap-1.5">
              <Stat label="used" value={compact(used)} tone={tone} />
              <Stat label="window" value={compact(window_)} />
              <Stat label="free" value={compact(Math.max(0, window_ - used))} />
              {fraction >= 0.7 && (
                <p className="text-[0.625rem] leading-snug text-amber">
                  compaction is close — history will be summarised
                </p>
              )}
            </div>
          </div>
        ) : (
          <p className="text-xs text-disabled">
            <Gauge className="mr-1 inline size-3" />
            no reading yet — the first turn reports it
          </p>
        )}
      </Section>

      <Separator />

      <Section title="Tokens">
        <Meter
          total={Math.max(totalTokens, 1)}
          segments={[
            { value: input_tokens, className: "bg-info", label: "input" },
            { value: output_tokens, className: "bg-ember", label: "output" },
            { value: cache_read, className: "bg-success", label: "cache read" },
            { value: cache_write, className: "bg-plan", label: "cache write" },
          ]}
        />
        <div className="flex flex-col gap-1.5 pt-1">
          <Stat
            label="input"
            value={
              <span className="flex items-center gap-1">
                <ArrowUpFromLine className="size-3 text-info" />
                {compact(input_tokens)}
              </span>
            }
          />
          <Stat
            label="output"
            value={
              <span className="flex items-center gap-1">
                <ArrowDownToLine className="size-3 text-ember" />
                {compact(output_tokens)}
              </span>
            }
          />
          <Stat label="cache read" value={compact(cache_read)} tone="text-success" />
          <Stat label="cache write" value={compact(cache_write)} tone="text-plan" />
          {cacheHit !== null && (
            <Stat
              label="cache hit"
              value={
                <span className="flex items-center gap-1">
                  <DatabaseZap className="size-3" />
                  {cacheHit}%
                </span>
              }
              tone={cacheHit >= 50 ? "text-success" : "text-secondary"}
              hint="share of prompt tokens served from the provider's cache"
            />
          )}
        </div>
      </Section>

      <Separator />

      <Section title="Spend">
        <div className="flex items-baseline gap-2">
          <span className="font-mono text-2xl leading-none text-text tabular">
            {money(state.cost_usd)}
          </span>
          {state.unpriced_turns > 0 && (
            <Badge
              variant="warning"
              title="turns on a model with no price in the table — not included above"
            >
              +{state.unpriced_turns} unpriced
            </Badge>
          )}
        </div>
      </Section>

      <Separator />

      <Section title="Board">
        {state.tasks.length === 0 ? (
          <p className="text-xs text-disabled">no board on this session</p>
        ) : (
          <div className="flex flex-col gap-1.5">
            {BOARD_ROWS.map(({ status, label, icon: Icon, tone: rowTone }) => {
              const count = state.tasks.filter((task) => task.status === status).length;
              return (
                <Stat
                  key={status}
                  label={label}
                  value={
                    <span
                      className={cn(
                        "flex items-center gap-1.5",
                        count === 0 && "text-disabled",
                      )}
                    >
                      <Icon className={cn("size-3", count > 0 && rowTone)} />
                      {count}
                    </span>
                  }
                />
              );
            })}
          </div>
        )}
      </Section>

      <Separator />

      <Section title="Activity">
        <div className="flex flex-col gap-1.5">
          <Stat
            label="phase"
            value={
              <span className="flex items-center gap-1.5">
                <Dot
                  className={state.phase === "idle" ? "text-disabled" : "text-ember"}
                  pulse={state.phase !== "idle"}
                />
                {state.phase}
              </span>
            }
          />
          <Stat
            label="tool calls"
            value={
              <span className="flex items-center gap-1">
                <Wrench className="size-3" />
                {toolCalls}
              </span>
            }
          />
          {running?.kind === "tool_card" && (
            <Stat label="running" value={running.tool_name} tone="text-ember" />
          )}
          {state.plan_gated && (
            <div className="rounded-lg border border-plan/30 bg-plan/10 px-2 py-1.5 text-[0.6875rem] text-plan">
              plan gate is up — every tool above read-only is refused
            </div>
          )}
        </div>
      </Section>

      <Separator />

      <Section title="Runtime" className="pb-2">
        <div className="flex flex-col gap-1.5">
          <Stat label="provider" value={state.provider} />
          <Stat
            label="model"
            value={<span className="block max-w-36 truncate">{state.model}</span>}
            hint={state.model}
          />
          {meta && (
            <Stat label="uptime" value={duration(now - meta.started_at_ms)} />
          )}
          <Stat label="events" value={compact(state.seq)} hint="stream sequence number" />
        </div>
      </Section>
    </aside>
  );
}
