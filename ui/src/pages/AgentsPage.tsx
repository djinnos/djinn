import { AgentRoles } from "@/components/AgentRoles";
import { AgentMetricsDashboard } from "@/components/AgentMetricsDashboard";
import { McpServersManager } from "@/components/McpServersManager";
import { PageHeader } from "@/components/PageHeader";
import { useSelectedProject } from "@/stores/useProjectStore";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";

export function AgentsPage() {
  const project = useSelectedProject();

  return (
    <div className="flex h-full flex-col overflow-hidden p-6">
      <PageHeader
        title="Agents"
        subtitle={
          project
            ? `Showing agents for ${project.name}`
            : "Showing agents across all projects"
        }
        className="shrink-0"
      />
      <Tabs defaultValue="roles" className="flex flex-1 flex-col min-h-0">
        <TabsList className="shrink-0 w-fit">
          <TabsTrigger value="roles">Roles</TabsTrigger>
          <TabsTrigger value="mcp-servers">MCP Servers</TabsTrigger>
          <TabsTrigger value="metrics">Metrics</TabsTrigger>
        </TabsList>
        <TabsContent
          value="roles"
          className="flex-1 min-h-0 overflow-y-auto mt-4"
        >
          <AgentRoles />
        </TabsContent>
        <TabsContent
          value="mcp-servers"
          className="flex-1 min-h-0 overflow-y-auto mt-4"
        >
          <McpServersManager />
        </TabsContent>
        <TabsContent
          value="metrics"
          className="flex-1 min-h-0 overflow-y-auto mt-4"
        >
          <AgentMetricsDashboard
            projectId={project?.id ?? null}
            projectContextText={
              project
                ? `Metrics scoped to ${project.name}`
                : "Select a project to view metrics"
            }
          />
        </TabsContent>
      </Tabs>
    </div>
  );
}
