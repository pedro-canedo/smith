import { CircleCheck, CircleDashed, Eye, KanbanSquare, Loader2, OctagonX } from "lucide-react";
import type { Task, TaskStatus } from "@/lib/types";
import { ago } from "@/lib/format";
import { cn } from "@/lib/utils";

const COLUMNS: {
  status: TaskStatus;
  title: string;
  icon: typeof CircleDashed;
  accent: string;
  rail: string;
}[] = [
  {
    status: "pending",
    title: "Backlog",
    icon: CircleDashed,
    accent: "text-secondary",
    rail: "bg-secondary/40",
  },
  {
    status: "in_progress",
    title: "Doing",
    icon: Loader2,
    accent: "text-ember",
    rail: "bg-ember",
  },
  {
    status: "blocked",
    title: "Blocked",
    icon: OctagonX,
    accent: "text-danger",
    rail: "bg-danger",
  },
  { status: "review", title: "Review", icon: Eye, accent: "text-amber", rail: "bg-amber" },
  {
    status: "completed",
    title: "Done",
    icon: CircleCheck,
    accent: "text-success",
    rail: "bg-success/60",
  },
];

export function Board({ tasks }: { tasks: Task[] }) {
  if (tasks.length === 0) {
    return (
      <div className="flex flex-col items-center gap-2 py-16 text-center">
        <KanbanSquare className="size-6 text-disabled" />
        <p className="text-sm text-secondary">No board on this session yet.</p>
        <p className="max-w-sm text-xs text-disabled">
          smith opens one itself when a task takes several steps — it is the same
          list the terminal shows.
        </p>
      </div>
    );
  }

  return (
    <div className="grid grid-cols-1 gap-4 sm:grid-cols-2 xl:grid-cols-5">
      {COLUMNS.map(({ status, title, icon: Icon, accent, rail }) => {
        const cards = tasks.filter((task) => task.status === status);
        return (
          <section key={status} className="flex min-w-0 flex-col gap-2">
            <header className="flex items-center gap-1.5 px-0.5">
              <Icon className={cn("size-3.5", accent)} />
              <h3 className={cn("text-xs font-semibold", accent)}>{title}</h3>
              <span className="ml-auto font-mono text-[0.625rem] text-disabled tabular">
                {cards.length}
              </span>
            </header>
            <div className="flex flex-col gap-2">
              {cards.length === 0 && (
                <div className="rounded-panel border border-dashed border-text/8 px-3 py-4 text-center text-[0.6875rem] text-disabled">
                  empty
                </div>
              )}
              {cards.map((task) => (
                <article
                  key={task.id ?? task.content}
                  className="panel relative overflow-hidden px-3 py-2.5 pl-4"
                >
                  <span className={cn("absolute inset-y-0 left-0 w-1", rail)} />
                  <p
                    className={cn(
                      "text-sm leading-snug",
                      status === "completed" && "text-secondary line-through",
                    )}
                  >
                    {task.content}
                  </p>
                  {task.blocked_reason && (
                    <p className="mt-1.5 rounded-md bg-danger/10 px-2 py-1 text-[0.6875rem] leading-snug text-danger">
                      {task.blocked_reason}
                    </p>
                  )}
                  {task.updated_at && (
                    <p className="mt-1.5 font-mono text-[0.625rem] text-disabled">
                      {ago(task.updated_at)}
                    </p>
                  )}
                </article>
              ))}
            </div>
          </section>
        );
      })}
    </div>
  );
}
