import { useState } from "react";
import {
  Boxes,
  Check,
  ChevronsLeft,
  ChevronsRight,
  Copy,
  ExternalLink,
  Flame,
  Github,
  KanbanSquare,
  MessageSquare,
  PanelsTopLeft,
  ScrollText,
  Search,
  ServerCog,
  ShieldQuestion,
  Sparkles,
  Waypoints,
} from "lucide-react";
import type { ConsoleLink, ConsoleMeta, SessionProjection } from "@/lib/types";
import { shortId, shortPath } from "@/lib/format";
import { Badge, Dot } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";

export type View = "session" | "board" | "history";

/** Endpoint icons, keyed on the id the server mints (links.rs). An id with no
 * entry gets the generic one rather than nothing — the rail must render a
 * link smith learns about in a later version. */
const LINK_ICONS: Record<string, typeof Waypoints> = {
  "9router": Waypoints,
  ollama: Boxes,
  openrouter: ServerCog,
  anthropic: Sparkles,
  openai: Sparkles,
  searxng: Search,
  repo: Github,
};

const GROUP_TITLES: Record<ConsoleLink["group"], string> = {
  provider: "Providers",
  service: "Services",
  reference: "Reference",
};

function LinkRow({ link, collapsed }: { link: ConsoleLink; collapsed: boolean }) {
  const Icon = LINK_ICONS[link.id] ?? Waypoints;
  return (
    <a
      href={link.url}
      target="_blank"
      // noreferrer, not just noopener: this page's URL carries the session
      // token in its query string (links.rs and index.html say the same).
      rel="noreferrer"
      title={collapsed ? `${link.label} — ${link.detail}` : link.url}
      className={cn(
        "group flex items-center gap-2.5 rounded-lg px-2.5 py-1.5 text-sm transition-colors",
        "text-secondary hover:bg-hover hover:text-text",
        collapsed && "justify-center px-0",
      )}
    >
      <span className="relative">
        <Icon className={cn("size-4", link.active && "text-ember")} />
        {link.active && (
          <span className="absolute -right-0.5 -bottom-0.5 size-1.5 rounded-full bg-ember ring-2 ring-base" />
        )}
      </span>
      {!collapsed && (
        <>
          <span className="min-w-0 flex-1 truncate">{link.label}</span>
          <span className="truncate font-mono text-[0.625rem] text-disabled opacity-0 transition-opacity group-hover:opacity-100">
            {link.detail}
          </span>
          {link.external && (
            <ExternalLink className="size-3 shrink-0 text-disabled" />
          )}
        </>
      )}
    </a>
  );
}

function NavRow({
  icon: Icon,
  label,
  count,
  active,
  collapsed,
  onClick,
}: {
  icon: typeof MessageSquare;
  label: string;
  count?: number;
  active: boolean;
  collapsed: boolean;
  onClick: () => void;
}) {
  return (
    <button
      onClick={onClick}
      title={collapsed ? label : undefined}
      aria-current={active ? "page" : undefined}
      className={cn(
        "relative flex cursor-pointer items-center gap-2.5 rounded-lg px-2.5 py-2 text-sm transition-colors",
        active
          ? "bg-ember/10 font-medium text-ember"
          : "text-secondary hover:bg-hover hover:text-text",
        collapsed && "justify-center px-0",
      )}
    >
      {active && (
        <span className="absolute top-1/2 left-0 h-4 w-0.5 -translate-y-1/2 rounded-r bg-ember" />
      )}
      <Icon className="size-4" />
      {!collapsed && (
        <>
          <span className="flex-1 text-left">{label}</span>
          {count !== undefined && count > 0 && (
            <span className="font-mono text-[0.625rem] text-disabled tabular">{count}</span>
          )}
        </>
      )}
    </button>
  );
}

export function Rail({
  view,
  onView,
  state,
  meta,
  collapsed,
  onToggle,
  connected,
}: {
  view: View;
  onView: (view: View) => void;
  state: SessionProjection | null;
  meta: ConsoleMeta | null;
  collapsed: boolean;
  onToggle: () => void;
  connected: boolean;
}) {
  const [copied, setCopied] = useState(false);
  const pendingAsks =
    (state?.pending_permission ? 1 : 0) + (state?.pending_question ? 1 : 0);
  const openTasks =
    state?.tasks.filter((task) => task.status !== "completed").length ?? 0;

  const groups: ConsoleLink["group"][] = ["provider", "service", "reference"];

  const copyId = () => {
    if (!meta) return;
    void navigator.clipboard?.writeText(meta.session_id).then(() => {
      setCopied(true);
      window.setTimeout(() => setCopied(false), 1200);
    });
  };

  return (
    <aside
      className={cn(
        "flex h-full shrink-0 flex-col gap-4 border-r border-text/8 bg-raised/40 py-4",
        "transition-[width] duration-200",
        collapsed ? "w-[3.75rem] px-2" : "w-60 px-3",
      )}
    >
      <div className={cn("flex items-center gap-2.5", collapsed && "justify-center")}>
        <span className="grid size-8 shrink-0 place-items-center rounded-lg bg-ember/12 ring-1 ring-ember/25">
          <Flame className="size-4 text-ember" />
        </span>
        {!collapsed && (
          <div className="min-w-0 flex-1">
            <div className="flex items-baseline gap-1.5">
              <span className="font-semibold tracking-tight text-text">smith</span>
              <span className="font-mono text-[0.625rem] text-disabled">
                {meta ? `v${meta.version}` : ""}
              </span>
            </div>
            <div className="flex items-center gap-1.5 text-[0.6875rem] text-disabled">
              <Dot
                className={connected ? "text-success" : "text-danger"}
                pulse={!connected}
              />
              {connected ? "connected" : "reconnecting"}
            </div>
          </div>
        )}
      </div>

      <nav className="flex flex-col gap-0.5">
        {!collapsed && <h2 className="eyebrow px-2.5 pb-1">Workspace</h2>}
        <NavRow
          icon={MessageSquare}
          label="Session"
          active={view === "session"}
          collapsed={collapsed}
          onClick={() => onView("session")}
        />
        <NavRow
          icon={KanbanSquare}
          label="Board"
          count={openTasks}
          active={view === "board"}
          collapsed={collapsed}
          onClick={() => onView("board")}
        />
        <NavRow
          icon={ScrollText}
          label="History"
          active={view === "history"}
          collapsed={collapsed}
          onClick={() => onView("history")}
        />
      </nav>

      {pendingAsks > 0 && (
        <button
          onClick={() => onView("session")}
          title={collapsed ? "waiting on you" : undefined}
          className={cn(
            "flex cursor-pointer items-center gap-2 rounded-lg border border-warning/30 bg-warning/10",
            "px-2.5 py-2 text-left text-xs text-warning transition-colors hover:bg-warning/20",
            collapsed && "justify-center px-0",
          )}
        >
          <ShieldQuestion className="size-4 shrink-0 breathe" />
          {!collapsed && <span className="flex-1">waiting on you</span>}
          {!collapsed && <Badge variant="warning">{pendingAsks}</Badge>}
        </button>
      )}

      <div className="min-h-0 flex-1 overflow-y-auto">
        {groups.map((group) => {
          const links = meta?.links.filter((link) => link.group === group) ?? [];
          if (links.length === 0) return null;
          return (
            <div key={group} className="mb-3 flex flex-col gap-0.5">
              {!collapsed && (
                <h2 className="eyebrow px-2.5 pb-1">{GROUP_TITLES[group]}</h2>
              )}
              {collapsed && <div className="mx-auto mb-1 h-px w-6 bg-text/8" />}
              {links.map((link) => (
                <LinkRow key={link.id} link={link} collapsed={collapsed} />
              ))}
            </div>
          );
        })}
      </div>

      <div className="flex flex-col gap-2 border-t border-text/8 pt-3">
        {!collapsed && meta && (
          <div className="px-1">
            <div className="eyebrow pb-1">Session</div>
            <button
              onClick={copyId}
              title={`${meta.session_id} — click to copy`}
              className="flex w-full cursor-pointer items-center gap-1.5 rounded px-1 py-0.5 font-mono text-[0.6875rem] text-secondary hover:bg-hover hover:text-text"
            >
              <span className="flex-1 truncate text-left">
                {shortId(meta.session_id)}
              </span>
              {copied ? (
                <Check className="size-3 text-success" />
              ) : (
                <Copy className="size-3 text-disabled" />
              )}
            </button>
            <div
              className="truncate px-1 font-mono text-[0.625rem] text-disabled"
              title={meta.cwd}
            >
              {shortPath(meta.cwd)}
            </div>
          </div>
        )}
        <Button
          variant="ghost"
          size={collapsed ? "icon-sm" : "sm"}
          onClick={onToggle}
          title={collapsed ? "expand" : "collapse"}
          className={cn(!collapsed && "justify-start")}
        >
          {collapsed ? (
            <ChevronsRight />
          ) : (
            <>
              <ChevronsLeft />
              <PanelsTopLeft className="size-3.5" />
              collapse
            </>
          )}
        </Button>
      </div>
    </aside>
  );
}
