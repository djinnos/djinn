import { useTaskStore } from '@/stores/taskStore';
import { useEpicStore } from '@/stores/epicStore';
import { EmptyState } from '@/components/EmptyState';
import { KanbanBoard } from '@/components/KanbanBoard';

export function KanbanPage() {
  const { tasks } = useTaskStore();
  const { epics } = useEpicStore();

  const noTasks = tasks.size === 0;
  const noEpics = epics.size === 0;

  if (noTasks) {
    return (
      <div className="flex h-full items-center justify-center p-6">
        <EmptyState
          title="No tasks yet"
          message="Create your first task to start tracking work on the board."
          actionLabel="Create first task"
          onAction={() => window.location.assign('/settings/projects')}
          illustration={<div className="text-4xl">📝</div>}
        />
      </div>
    );
  }

  if (noEpics) {
    return (
      <div className="flex h-full items-center justify-center p-6">
        <EmptyState
          title="No epics yet"
          message="Create an epic to group related tasks and plan larger goals."
          actionLabel="Create first epic"
          onAction={() => window.location.assign('/roadmap')}
          illustration={<div className="text-4xl">🗺️</div>}
        />
      </div>
    );
  }

  return <KanbanBoard />
  );
}
