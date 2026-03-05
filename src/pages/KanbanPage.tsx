import { useTaskStore } from '@/stores/taskStore';
import { useEpicStore } from '@/stores/epicStore';
import { EmptyState } from '@/components/EmptyState';

export function KanbanPage() {
  const { tasks } = useTaskStore();
  const { epics } = useEpicStore();

  const noTasks = tasks.size === 0;
  const noEpics = epics.size === 0;

  return (
    <div className="flex h-full min-w-0 flex-col p-6">
      <div className="mb-6">
        <h1 className="text-2xl font-bold text-foreground">Kanban Board</h1>
        <p className="mt-1 text-muted-foreground">Manage your tasks and workflow</p>
      </div>

      <div className="flex-1 min-h-0">
        {noTasks ? (
          <EmptyState
            title="No tasks yet"
            message="Create your first task to start tracking work on the board."
            actionLabel="Create first task"
            onAction={() => window.location.assign('/settings/projects')}
            illustration={<div className="text-4xl">📝</div>}
          />
        ) : noEpics ? (
          <EmptyState
            title="No epics yet"
            message="Create an epic to group related tasks and plan larger goals."
            actionLabel="Create first epic"
            onAction={() => window.location.assign('/roadmap')}
            illustration={<div className="text-4xl">🗺️</div>}
          />
        ) : (
          <div className="flex h-full items-center justify-center rounded-lg border border-dashed border-border bg-card/50 p-8">
            <div className="text-center">
              <p className="text-muted-foreground">Kanban board coming soon</p>
              <p className="mt-2 text-xs text-muted-foreground/60">{tasks.size} tasks · {epics.size} epics</p>
            </div>
          </div>
        )}
      </div>
    </div>
  );
}
