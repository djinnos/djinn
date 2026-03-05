import type { Task } from "@/types";
import { PRIORITIES, STATUS_COLUMNS, getEpicEmoji, getEpicTitle, useKanbanData } from "@/hooks/useKanbanData";
import { TaskCard } from "@/components/TaskCard";
import { TaskDetailPanel } from "@/components/TaskDetailPanel";

export function KanbanBoard() {
  const {
    epics,
    collapsedEpics,
    movingTaskIds,
    epicFilter,
    ownerFilter,
    priorityFilters,
    searchInput,
    selectedTask,
    epicOptions,
    ownerOptions,
    groupedByStatusThenEpic,
    setEpicFilter,
    setOwnerFilter,
    setPriorityFilters,
    setSearchInput,
    setSelectedTask,
    toggleEpic,
  } = useKanbanData();

  return (
    <div className="flex h-full min-h-0 flex-col gap-4">
      <div className="flex flex-wrap items-center gap-3 rounded-lg border bg-card p-3">
        <select
          value={epicFilter}
          onChange={(e) => setEpicFilter(e.target.value)}
          className="rounded border bg-background px-2 py-1 text-sm"
        >
          <option value="">All epics</option>
          {epicOptions.map((epic) => (
            <option key={epic.id} value={epic.id}>
              {getEpicEmoji(epic)} {epic.title}
            </option>
          ))}
        </select>

        <div className="flex items-center gap-1">
          {PRIORITIES.map((priority) => {
            const active = priorityFilters.includes(priority);
            return (
              <button
                key={priority}
                type="button"
                onClick={() =>
                  setPriorityFilters((prev) =>
                    prev.includes(priority)
                      ? prev.filter((p) => p !== priority)
                      : [...prev, priority]
                  )
                }
                className={`rounded-full border px-2 py-1 text-xs ${active ? "bg-primary text-primary-foreground" : "bg-background"}`}
              >
                {priority}
              </button>
            );
          })}
        </div>

        <select
          value={ownerFilter}
          onChange={(e) => setOwnerFilter(e.target.value)}
          className="rounded border bg-background px-2 py-1 text-sm"
        >
          <option value="">All owners</option>
          {ownerOptions.map((owner) => (
            <option key={owner} value={owner}>
              {owner}
            </option>
          ))}
        </select>

        <input
          value={searchInput}
          onChange={(e) => setSearchInput(e.target.value)}
          placeholder="Search tasks..."
          className="min-w-[220px] rounded border bg-background px-2 py-1 text-sm"
        />
      </div>

      <div className="flex min-h-0 flex-1 gap-4 overflow-x-auto pb-1">
        {STATUS_COLUMNS.map((column) => {
          const statusMap = groupedByStatusThenEpic.get(column.key) ?? new Map<string, Task[]>();
          const epicGroups = Array.from(statusMap.entries());
          const taskCount = epicGroups.reduce((total, [, epicTasks]) => total + epicTasks.length, 0);

          return (
            <section
              key={column.key}
              className="flex min-w-[260px] flex-1 flex-col rounded-lg border bg-card transition-all duration-300 ease-in-out"
            >
              <header className={`border-b-2 px-3 py-2 text-sm font-semibold ${column.accentClass}`}>
                {column.label} {taskCount}
              </header>

              <div className="flex-1 overflow-y-auto p-3">
                <div className="flex flex-col gap-3">
                  {epicGroups.map(([epicKey, epicTasks]) => {
                    const firstTaskEpicId = epicTasks[0]?.epicId ?? null;
                    const epic = firstTaskEpicId ? epics.get(firstTaskEpicId) : undefined;
                    const collapseKey = `${column.key}:${epicKey}`;
                    const isCollapsed = !!collapsedEpics[collapseKey];

                    return (
                      <div key={epicKey} className="rounded-md border bg-background transition-all duration-300 ease-in-out">
                        <button
                          type="button"
                          onClick={() => toggleEpic(column.key, epicKey)}
                          className="flex w-full items-center justify-between gap-2 border-b px-2 py-1 text-left text-sm font-medium"
                        >
                          <span className="truncate">
                            {getEpicEmoji(epic)} {getEpicTitle(epic, firstTaskEpicId)}
                          </span>
                          <span>{isCollapsed ? "▸" : "▾"}</span>
                        </button>

                        {!isCollapsed && (
                          <ul className="flex flex-col gap-2 p-2 transition-all duration-300 ease-in-out">
                            {epicTasks.map((task) => (
                              <li key={task.id}>
                                <TaskCard
                                  task={task}
                                  epic={task.epicId ? epics.get(task.epicId) : undefined}
                                  moving={!!movingTaskIds[task.id]}
                                  onClick={() => setSelectedTask(task)}
                                />
                              </li>
                            ))}
                          </ul>
                        )}
                      </div>
                    );
                  })}
                </div>
              </div>
            </section>
          );
        })}
      </div>

      <TaskDetailPanel
        open={!!selectedTask}
        task={selectedTask}
        epic={selectedTask?.epicId ? epics.get(selectedTask.epicId) : undefined}
        onClose={() => setSelectedTask(null)}
      />
    </div>
  );
}
