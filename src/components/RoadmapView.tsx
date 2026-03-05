/**
 * RoadmapView page component - Lists all epics with their progress
 */

import { useMemo, useState } from "react";
import { EpicCard } from "./EpicCard";
import { TaskDetailPanel } from "./TaskDetailPanel";
import { useAllEpics } from "@/stores/useEpicStore";
import type { Epic, Task } from "@/types";
import { Button } from "@/components/ui/button";

function sortEpics(epics: Epic[]): Epic[] {
  const priorityOrder: Record<Epic["priority"], number> = { P0: 0, P1: 1, P2: 2, P3: 3 };
  return [...epics].sort((a, b) => {
    const priorityDiff = priorityOrder[a.priority] - priorityOrder[b.priority];
    if (priorityDiff !== 0) return priorityDiff;
    return new Date(b.createdAt).getTime() - new Date(a.createdAt).getTime();
  });
}

function getEpicEmoji(epicId: string): string {
  const emojis = ["🚀", "🎯", "⭐", "🔥", "💎", "🎨", "⚡", "🔧", "📊", "🎪", "🏆", "🌟"];
  let hash = 0;
  for (let i = 0; i < epicId.length; i++) {
    const char = epicId.charCodeAt(i);
    hash = ((hash << 5) - hash) + char;
    hash = hash & hash;
  }
  return emojis[Math.abs(hash) % emojis.length];
}

export function RoadmapView() {
  const epics = useAllEpics();
  const sortedEpics = useMemo(() => sortEpics(epics), [epics]);
  const [expandAllSignal, setExpandAllSignal] = useState(0);
  const [collapseAllSignal, setCollapseAllSignal] = useState(0);
  const [selectedTask, setSelectedTask] = useState<Task | null>(null);

  if (epics.length === 0) {
    return <div className="flex flex-col items-center justify-center p-8 text-center">No Epics Yet</div>;
  }

  return (
    <div className="space-y-4 p-4">
      <div className="flex items-center justify-between">
        <h1 className="text-xl font-semibold">Roadmap</h1>
        <div className="flex items-center gap-2">
          <Button variant="outline" size="sm" onClick={() => setExpandAllSignal((n) => n + 1)}>Expand all</Button>
          <Button variant="outline" size="sm" onClick={() => setCollapseAllSignal((n) => n + 1)}>Collapse all</Button>
        </div>
      </div>

      <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-3 xl:grid-cols-4">
        {sortedEpics.map((epic) => (
          <EpicCard
            key={epic.id}
            epic={epic}
            emoji={getEpicEmoji(epic.id)}
            expandAllSignal={expandAllSignal}
            collapseAllSignal={collapseAllSignal}
            onTaskClick={setSelectedTask}
          />
        ))}
      </div>

      <TaskDetailPanel task={selectedTask} open={selectedTask !== null} onClose={() => setSelectedTask(null)} />
    </div>
  );
}
