import { memo, useState } from "react";
import { ListChecks, Square, X } from "lucide-react";

import { Button } from "@/components/ui/button";
import type { TaskSummary, TaskResult } from "@/lib/protocol";
import { cn, formatClockTime } from "@/lib/utils";
import {
  getBackgroundTaskResult,
  killBackgroundTask,
  selectActiveBackgroundTasks,
  useCodaStore,
} from "@/store/session";

/** Running first, then most recently started — the ones still worth watching
 * stay at the top as older ones settle beneath them. */
export function orderTasks(tasks: TaskSummary[]): TaskSummary[] {
  const sorted = [...tasks].sort((a, b) => {
    if (a.subtree_active !== b.subtree_active) {
      return a.subtree_active ? -1 : 1;
    }
    return b.started_at.localeCompare(a.started_at);
  });
  const ids = new Set(tasks.map((task) => task.id));
  return sorted
    .filter((task) => !task.parent_task_id || !ids.has(task.parent_task_id))
    .flatMap((parent) => [parent, ...sorted.filter((child) => child.parent_task_id === parent.id)]);
}

function TaskRow({ task }: { task: TaskSummary }) {
  const started = formatClockTime(task.started_at);
  const label = task.kind.kind === "subagent" ? task.kind.agent_name : task.command;
  const [result, setResult] = useState<TaskResult | null>(null);
  const [loading, setLoading] = useState(false);
  const showResult = async () => {
    setLoading(true);
    try {
      setResult(await getBackgroundTaskResult(task.id));
    } catch (error) {
      setResult({
        state: "error",
        message: error instanceof Error ? error.message : "Could not load result",
      });
    } finally {
      setLoading(false);
    }
  };
  return (
    <li
      className={cn(
        "rounded-md border border-border/60 px-2.5 py-2",
        task.parent_task_id && "ml-4",
      )}
    >
      <div className="flex items-center gap-2">
        <span
          aria-hidden
          className={cn(
            "size-1.5 shrink-0 rounded-full",
            task.running ? "animate-pulse bg-primary" : "bg-muted-foreground/40",
          )}
        />
        <span className="min-w-0 flex-1 truncate font-mono text-xs" title={label}>
          {label}
        </span>
        {task.subtree_active ? (
          <Button
            variant="ghost"
            size="icon"
            className="size-6 shrink-0 text-muted-foreground hover:text-destructive"
            onClick={() => killBackgroundTask(task.id)}
            title="Stop this task"
            aria-label={`Stop ${label}`}
          >
            <Square className="size-3" />
          </Button>
        ) : null}
      </div>
      {task.description ? (
        <p className="mt-1 truncate text-xs text-muted-foreground" title={task.description}>
          {task.description}
        </p>
      ) : null}
      <div className="mt-1 flex items-center gap-2 text-[0.6875rem] text-muted-foreground">
        <span className={cn(task.running && "text-foreground")}>{task.status}</span>
        {started ? <span>· started {started}</span> : null}
        {/* Only worth naming when it wasn't the session's own agent. */}
        {task.agent_name && task.agent_name !== "coda" ? <span>· {task.agent_name}</span> : null}
      </div>
      {task.kind.kind === "subagent" && !task.running ? (
        <div className="mt-2">
          <Button variant="ghost" size="sm" disabled={loading} onClick={showResult}>
            {loading ? "Loading…" : result ? "Refresh result" : "View result"}
          </Button>
          {result ? (
            <div className="mt-1 max-h-72 overflow-auto whitespace-pre-wrap break-words text-xs">
              {result.state === "available"
                ? result.answer
                : result.state === "expired"
                  ? "Result expired"
                  : result.state === "unknown"
                    ? "Task not found"
                    : result.state === "error"
                      ? result.message
                      : result.status}
            </div>
          ) : null}
        </div>
      ) : null}
    </li>
  );
}

function TaskList({ tasks }: { tasks: TaskSummary[] }) {
  if (tasks.length === 0) {
    return (
      <p className="px-1 py-6 text-center text-xs text-muted-foreground">
        No background tasks yet. Background agents and shell commands can keep working alongside the
        conversation.
      </p>
    );
  }
  return (
    <ul className="flex flex-col gap-1.5">
      {tasks.map((task) => (
        <TaskRow key={task.id} task={task} />
      ))}
    </ul>
  );
}

function PanelHeader({ onClose }: { onClose: () => void }) {
  return (
    <div className="flex shrink-0 items-center gap-2 pb-2">
      <ListChecks className="size-4 shrink-0 text-muted-foreground" />
      <h2 className="min-w-0 flex-1 truncate text-sm font-medium">Background tasks</h2>
      <Button
        variant="ghost"
        size="icon"
        className="size-6"
        onClick={onClose}
        title="Close background tasks"
        aria-label="Close background tasks"
      >
        <X className="size-4" />
      </Button>
    </div>
  );
}

/**
 * The session's background tasks. One list, two shapes: a column beside the
 * transcript where there is room for it, and a sheet up from the bottom where
 * there isn't — a side panel on a phone would leave the transcript unreadably
 * narrow.
 */
export const BackgroundTasksPanel = memo(function BackgroundTasksPanel({
  open,
  onClose,
}: {
  open: boolean;
  onClose: () => void;
}) {
  const tasks = orderTasks(useCodaStore(selectActiveBackgroundTasks));

  return (
    <>
      {/* Wide: part of the layout, so the transcript reflows rather than
          being covered by something the user then has to dismiss. */}
      {open ? (
        <aside className="hidden w-[20rem] shrink-0 flex-col overflow-hidden rounded-lg border bg-background p-2.5 lg:flex">
          <PanelHeader onClose={onClose} />
          <div className="min-h-0 flex-1 overflow-y-auto">
            <TaskList tasks={tasks} />
          </div>
        </aside>
      ) : null}

      {/* Narrow: a bottom sheet. Kept mounted so it can animate out. */}
      {open ? (
        <button
          type="button"
          className="fixed inset-0 z-40 bg-background/70 backdrop-blur-sm lg:hidden"
          onClick={onClose}
          aria-label="Close background tasks"
        />
      ) : null}
      <div
        aria-hidden={!open}
        inert={!open ? true : undefined}
        className={cn(
          "fixed inset-x-0 bottom-0 z-50 flex max-h-[70dvh] flex-col rounded-t-xl border-t bg-background p-2.5 pb-[calc(0.625rem+env(safe-area-inset-bottom))] transition-transform duration-200 lg:hidden",
          open ? "translate-y-0" : "translate-y-full",
        )}
      >
        <PanelHeader onClose={onClose} />
        <div className="min-h-0 flex-1 overflow-y-auto">
          <TaskList tasks={tasks} />
        </div>
      </div>
    </>
  );
});
