import { AgentMetricsDashboard } from '@/components/AgentMetricsDashboard';
import { getCurrentWindow } from '@tauri-apps/api/window';
import { useSelectedProject } from '@/stores/useProjectStore';

export function MetricsPage() {
  const project = useSelectedProject();

  return (
    <div className="flex h-full flex-col overflow-hidden p-6">
      <div
        className="mb-6 shrink-0 cursor-default select-none"
        onMouseDown={(e) => { if (e.button === 0 && e.target === e.currentTarget) void getCurrentWindow().startDragging(); }}
      >
        <h1
          className="text-2xl font-bold text-foreground"
          onMouseDown={(e) => { if (e.button === 0) void getCurrentWindow().startDragging(); }}
        >Metrics</h1>
        <p
          className="mt-1 text-muted-foreground"
          onMouseDown={(e) => { if (e.button === 0) void getCurrentWindow().startDragging(); }}
        >Agent effectiveness by role for this project</p>
      </div>
      <div className="flex-1 min-h-0 overflow-y-auto">
        <AgentMetricsDashboard projectId={project?.id ?? null} />
      </div>
    </div>
  );
}
