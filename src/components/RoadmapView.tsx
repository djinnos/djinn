/**
 * RoadmapView page component - Lists all epics with their progress
 */

import { useMemo, useState } from "react";
import { EpicCard } from "./EpicCard";
import { TaskDetailPanel } from "./TaskDetailPanel";
import { useAllEpics } from "@/stores/useEpicStore";
import type { Epic, Task } from "@/api/types";
import { Button } from "@/components/ui/button";

function sortEpics(epics: Epic[]): Epic[] {
  return [...epics].sort((a, b) => {
    return new Date(b.created_at).getTime() - new Date(a.created_at).getTime();
  });
}


interface RoadmapViewProps {
  mockEpics?: Epic[];
  mockTasks?: Task[];
}

export function RoadmapView({ mockEpics, mockTasks }: RoadmapViewProps = {}) {
  const storeEpics = useAllEpics();
  const epics = mockEpics ?? storeEpics;
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
            emoji={epic.emoji}
            expandAllSignal={expandAllSignal}
            collapseAllSignal={collapseAllSignal}
            onTaskClick={setSelectedTask}
            mockTasks={mockTasks?.filter((task) => task.epic_id === epic.id)}
          />
        ))}
      </div>

      <TaskDetailPanel task={selectedTask} open={selectedTask !== null} onClose={() => setSelectedTask(null)} />
    </div>
  );
}
