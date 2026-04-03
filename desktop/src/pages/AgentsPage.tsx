import { AgentRoles } from '@/components/AgentRoles';

export function AgentsPage() {
  return (
    <div className="flex h-full flex-col overflow-hidden p-6">
      <div className="flex-1 min-h-0 overflow-y-auto">
        <AgentRoles />
      </div>
    </div>
  );
}
