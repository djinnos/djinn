import { useTaskStore } from '@/stores/taskStore';
import { useEpicStore } from '@/stores/epicStore';

export function KanbanPage() {
  const { tasks } = useTaskStore();
  const { epics } = useEpicStore();

  return (
    <div className="flex h-full flex-col p-6">
      <div className="mb-6">
        <h1 className="text-2xl font-bold text-foreground">Kanban Board</h1>
        <p className="text-muted-foreground mt-1">
          Manage your tasks and workflow
        </p>
      </div>
      
      <div className="flex-1 rounded-lg border border-dashed border-border bg-card/50 p-8">
        <div className="flex h-full items-center justify-center">
          <div className="text-center">
            <p className="text-muted-foreground">Kanban board coming soon</p>
            <p className="text-xs text-muted-foreground/60 mt-2">
              {tasks.size} tasks · {epics.size} epics
            </p>
          </div>
        </div>
      </div>
    </div>
  );
}
