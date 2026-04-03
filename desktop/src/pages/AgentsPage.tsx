import { AgentRoles } from '@/components/AgentRoles';
import { AgentMetricsDashboard } from '@/components/AgentMetricsDashboard';
import { useSelectedProject } from '@/stores/useProjectStore';
import { Tabs, TabsContent, TabsList, TabsTrigger } from '@/components/ui/tabs';

export function AgentsPage() {
  const project = useSelectedProject();

  return (
    <div className="flex h-full flex-col overflow-hidden p-6">
      <Tabs defaultValue="roles" className="flex flex-1 flex-col min-h-0">
        <TabsList className="shrink-0 w-fit">
          <TabsTrigger value="roles">Roles</TabsTrigger>
          <TabsTrigger value="metrics">Metrics</TabsTrigger>
        </TabsList>
        <TabsContent value="roles" className="flex-1 min-h-0 overflow-y-auto mt-4">
          <AgentRoles />
        </TabsContent>
        <TabsContent value="metrics" className="flex-1 min-h-0 overflow-y-auto mt-4">
          <AgentMetricsDashboard projectId={project?.id ?? null} />
        </TabsContent>
      </Tabs>
    </div>
  );
}
