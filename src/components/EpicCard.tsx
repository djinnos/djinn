/**
 * EpicCard component - Displays epic information with progress bar and task count
 * 
 * Shows: emoji, title, colored border/accent, progress bar (closed/total %), task count badge
 */

import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Badge } from "@/components/ui/badge";
import { useTasksByEpic } from "@/stores/useTaskStore";
import type { Epic, Task } from "@/types";

interface EpicCardProps {
  epic: Epic;
  emoji?: string;
}

/**
 * Get border color class based on epic priority
 */
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

/**
 * Calculate progress percentage of closed tasks
 */
function calculateProgress(tasks: Task[]): { percentage: number; closed: number; total: number } {
  const total = tasks.length;
  if (total === 0) {
    return { percentage: 0, closed: 0, total: 0 };
  }
  const closed = tasks.filter((task) => task.status === "completed" || task.status === "canceled").length;
  const percentage = Math.round((closed / total) * 100);
  return { percentage, closed, total };
}

export function EpicCard({ epic, emoji = "🎯" }: EpicCardProps) {
  const tasks = useTasksByEpic(epic.id);
  const { percentage, closed, total } = calculateProgress(tasks);

  return (
    <Card className={`overflow-hidden ${getPriorityBorderColor(epic.priority)}`}>
      <CardHeader className="pb-2">
        <div className="flex items-start justify-between gap-2">
          <div className="flex items-center gap-2">
            <span className="text-lg" role="img" aria-label="epic emoji">
              {emoji}
            </span>
            <CardTitle className="line-clamp-2 text-sm font-medium">
              {epic.title}
            </CardTitle>
          </div>
          <Badge variant="secondary" className="shrink-0">
            {closed} / {total} done
          </Badge>
        </div>
      </CardHeader>
      <CardContent className="pt-0">
        {/* Progress bar */}
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
          <span className="text-xs font-medium text-muted-foreground w-10 text-right">
            {percentage}%
          </span>
        </div>
      </CardContent>
    </Card>
  );
}
