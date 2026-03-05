/**
 * EpicCard component - Displays epic information with progress bar and expandable task list
 */

import { useEffect, useState } from "react";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { useTasksByEpic } from "@/stores/useTaskStore";
import type { Epic, Task, TaskStatus } from "@/types";
import { ChevronDown } from "lucide-react";

interface EpicCardProps {
  mockTasks?: Task[];
  defaultExpanded?: boolean;
  epic: Epic;
  emoji?: string;
  expandAllSignal?: number;
  collapseAllSignal?: number;
  onTaskClick?: (task: Task) => void;
}

function getPriorityBorderColor(priority: Epic["priority"]): string {
  switch (priority) {
    case "P0":
      return "border-l-4 border-l-red-500";
    case "P1":
      return "border-l-4 border-l-orange-500";
    case "P2":
      return "border-l-4 border-l-blue-500";
    case "P3":
      return "border-l-4 border-l-gray-500";
    default:
      return "border-l-4 border-l-gray-500";
  }
}

function calculateProgress(tasks: Task[]): { percentage: number; closed: number; total: number } {
  const total = tasks.length;
  if (total === 0) return { percentage: 0, closed: 0, total: 0 };
  const closed = tasks.filter((task) => task.status === "completed").length;
  const percentage = Math.round((closed / total) * 100);
  return { percentage, closed, total };
}

function getStatusBadge(status: TaskStatus): { dot: string; label: string } {
  switch (status) {
    case "completed":
      return { dot: "bg-green-500", label: "Completed" };
    case "in_progress":
      return { dot: "bg-blue-500", label: "In Progress" };
    case "blocked":
      return { dot: "bg-red-500", label: "Blocked" };
    case "pending":
    default:
      return { dot: "bg-amber-500", label: "Pending" };
  }
}

export function EpicCard({
  epic,
  emoji = "🎯",
  expandAllSignal,
  collapseAllSignal,
  onTaskClick,
  mockTasks,
  defaultExpanded = false,
}: EpicCardProps) {
  const storeTasks = useTasksByEpic(epic.id);
  const tasks = mockTasks ?? storeTasks;
  const { percentage, closed, total } = calculateProgress(tasks);
  const [expanded, setExpanded] = useState(defaultExpanded);

  useEffect(() => {
    if (expandAllSignal !== undefined) setExpanded(true);
  }, [expandAllSignal]);

  useEffect(() => {
    if (collapseAllSignal !== undefined) setExpanded(false);
  }, [collapseAllSignal]);

  return (
    <Card className={`overflow-hidden ${getPriorityBorderColor(epic.priority)}`}>
      <button
        type="button"
        className="w-full text-left"
        onClick={() => setExpanded((value) => !value)}
        aria-expanded={expanded}
      >
        <CardHeader className="pb-2">
          <div className="flex items-start justify-between gap-2">
            <div className="flex items-center gap-2">
              <span className="text-lg" role="img" aria-label="epic emoji">{emoji}</span>
              <CardTitle className="line-clamp-2 text-sm font-medium">{epic.title}</CardTitle>
            </div>
            <div className="flex items-center gap-2">
              <Badge variant="secondary" className="shrink-0">
                {closed} / {total} done
              </Badge>
              <ChevronDown className={`h-4 w-4 transition-transform ${expanded ? "rotate-180" : ""}`} />
            </div>
          </div>
        </CardHeader>
      </button>

      <CardContent className="pt-0">
        <div className="flex items-center gap-3">
          <div className="relative h-2 flex-1 overflow-hidden rounded-full bg-muted">
            <div
              className="h-full rounded-full bg-primary transition-all duration-300 ease-out"
              style={{ width: `${percentage}%` }}
              role="progressbar"
              aria-valuenow={percentage}
              aria-valuemin={0}
              aria-valuemax={100}
              aria-label={`${percentage}% of tasks completed`}
            />
          </div>
          <span className="w-10 text-right text-xs font-medium text-muted-foreground">{percentage}%</span>
        </div>

        <div
          className={`grid transition-all duration-300 ease-in-out ${expanded ? "grid-rows-[1fr] opacity-100 mt-3" : "grid-rows-[0fr] opacity-0"}`}
        >
          <div className="min-h-0 overflow-hidden">
            {tasks.length === 0 ? (
              <p className="text-xs text-muted-foreground">No tasks yet.</p>
            ) : (
              <div className="space-y-1">
                {tasks.map((task) => {
                  const status = getStatusBadge(task.status);
                  return (
                    <Button
                      key={task.id}
                      variant="ghost"
                      className="h-auto w-full justify-start px-2 py-1.5"
                      onClick={(event) => {
                        event.stopPropagation();
                        onTaskClick?.(task);
                      }}
                    >
                      <span className={`mr-2 inline-block h-2 w-2 rounded-full ${status.dot}`} aria-hidden="true" />
                      <span className="mr-2 text-[11px] text-muted-foreground">{status.label}</span>
                      <span className="truncate text-sm">{task.title}</span>
                    </Button>
                  );
                })}
              </div>
            )}
          </div>
        </div>
      </CardContent>
    </Card>
  );
}
