import { SquareTerminal } from "lucide-react";
import { isRunningTask, taskStatusText, type TaskSummary } from "@/lib/protocol";
import { selectActiveBackgroundTasks, useCodaStore } from "@/store/session";
import { cn } from "@/lib/utils";

/**
 * Compact live overview of the session's background tasks, shown above the
 * composer while any task is retained. The list is server-pushed whole on
 * attach and on every change, so rendering it is a pure replacement.
 */
export function BackgroundTasksBar() {
  const tasks = useCodaStore(selectActiveBackgroundTasks);
  const running = tasks.filter((task) => isRunningTask(task.status));
  if (running.length === 0) {
    return null;
  }
  return (
    <div className="mx-auto w-full max-w-4xl px-4">
      <div className="mb-1.5 flex flex-col gap-1 rounded-md border border-border bg-card px-3 py-2 text-xs">
        <div className="flex items-center gap-2 font-medium text-muted-foreground">
          <SquareTerminal className="size-3.5" />
          <span>
            {running.length} background task{running.length > 1 ? "s" : ""} running
          </span>
        </div>
        {running.map((task) => (
          <TaskRow key={task.id} task={task} />
        ))}
      </div>
    </div>
  );
}

function TaskRow({ task }: { task: TaskSummary }) {
  const running = isRunningTask(task.status);
  return (
    <div className="flex min-w-0 items-center gap-2">
      <span
        className={cn(
          "size-1.5 shrink-0 rounded-full",
          running ? "animate-pulse bg-emerald-500" : "bg-muted-foreground/50",
        )}
      />
      <code className="shrink-0 text-muted-foreground">{task.id.slice(0, 11)}</code>
      <span className="truncate" title={task.command}>
        {task.description || task.command}
      </span>
      <span className="ml-auto shrink-0 text-muted-foreground">{taskStatusText(task.status)}</span>
    </div>
  );
}
