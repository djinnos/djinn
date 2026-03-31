import { AgentMetricsDashboard } from '@/components/AgentMetricsDashboard';
import { useSelectedProject } from '@/stores/useProjectStore';

export function MetricsPage() {
  const project = useSelectedProject();

  return (
    <div className="flex h-full flex-col overflow-hidden p-6">
      <div className="flex-1 min-h-0 overflow-y-auto">
        <AgentMetricsDashboard projectId={project?.id ?? null} />
      </div>
    </div>
  );
}
