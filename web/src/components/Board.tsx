import { CircleDashed, Loader2, OctagonX, Eye, CircleCheck } from "lucide-react";
import type { Task, TaskStatus } from "@/lib/types";
import { Card } from "@/components/ui/card";
import { Badge } from "@/components/ui/badge";
import { cn } from "@/lib/utils";

const COLUMNS: {
  status: TaskStatus;
  title: string;
  icon: typeof CircleDashed;
  accent: string;
}[] = [
  { status: "pending", title: "Backlog", icon: CircleDashed, accent: "text-secondary" },
  { status: "in_progress", title: "Doing", icon: Loader2, accent: "text-ember" },
  { status: "blocked", title: "Blocked", icon: OctagonX, accent: "text-danger" },
  { status: "review", title: "Review", icon: Eye, accent: "text-amber" },
  { status: "completed", title: "Done", icon: CircleCheck, accent: "text-success" },
];

function recency(updated_at?: number): string | null {
  if (!updated_at) return null;
  const seconds = Math.max(0, (Date.now() - updated_at) / 1000);
  if (seconds < 90) return "now";
  if (seconds < 3600) return `${Math.round(seconds / 60)}m`;
  return `${Math.round(seconds / 3600)}h`;
}

export function Board({ tasks }: { tasks: Task[] }) {
  if (tasks.length === 0) {
    return (
      <p className="text-sm text-disabled">
        No board yet — the agent creates one when a task takes several steps.
      </p>
    );
  }
  return (
    <div className="grid grid-cols-2 gap-3 lg:grid-cols-5">
      {COLUMNS.map(({ status, title, icon: Icon, accent }) => {
        const cards = tasks.filter((task) => task.status === status);
        return (
          <div key={status}>
            <div className={cn("mb-2 flex items-center gap-1.5 text-xs font-semibold", accent)}>
              <Icon className="size-3.5" />
              {title}
              <span className="text-disabled">{cards.length}</span>
            </div>
            <div className="flex flex-col gap-2">
              {cards.map((task) => (
                <Card key={task.id ?? task.content} className="px-3 py-2">
                  <div className="text-sm">{task.content}</div>
                  {task.blocked_reason && (
                    <div className="mt-1 text-xs text-danger">{task.blocked_reason}</div>
                  )}
                  {recency(task.updated_at) && (
                    <Badge className="mt-1">{recency(task.updated_at)}</Badge>
                  )}
                </Card>
              ))}
            </div>
          </div>
        );
      })}
    </div>
  );
}
