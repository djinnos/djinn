import { useCallback, useEffect, useState } from "react";
import { useSelectedProject } from "@/stores/useProjectStore";
import { Button } from "@/components/ui/button";
import { InlineError } from "@/components/InlineError";
import { useAuthUser } from "@/components/AuthGate";
import { AgentForm } from "@/components/agentRoles/AgentForm";
import { AgentCard } from "@/components/agentRoles/AgentCard";
import {
  type CreateAgentRequest,
  type Agent,
  type AvailableMcpServer,
  type AvailableSkill,
  createAgent,
  deleteAgent,
  fetchAvailableMcpServers,
  fetchAvailableSkills,
  fetchAgents,
  updateAgent,
} from "@/api/agents";

export { LearnedPromptSection } from "@/components/agentRoles/LearnedPromptSection";

// ── Main Component ───────────────────────────────────────────────────────────

export function AgentRoles() {
  const project = useSelectedProject();
  const isAdmin = useAuthUser()?.isAdmin ?? false;
  const [roles, setRoles] = useState<Agent[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  // Available MCP servers and skills for the project
  const [availableMcpServers, setAvailableMcpServers] = useState<AvailableMcpServer[]>([]);
  const [availableSkills, setAvailableSkills] = useState<AvailableSkill[]>([]);

  // Create form
  const [isCreating, setIsCreating] = useState(false);
  const [createBusy, setCreateBusy] = useState(false);

  // Edit state: role id → draft
  const [editingId, setEditingId] = useState<string | null>(null);
  const [editBusy, setEditBusy] = useState(false);

  // Deleting id
  const [deletingId, setDeletingId] = useState<string | null>(null);

  const loadRoles = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const data = await fetchAgents(project?.id);
      setRoles(data);
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to load agents");
    } finally {
      setLoading(false);
    }
  }, [project?.id]);

  // Load available MCP servers and skills when project changes or form opens
  const loadAvailableOptions = useCallback(async () => {
    if (!project?.id) return;
    try {
      const [servers, sk] = await Promise.all([
        fetchAvailableMcpServers(project.id),
        fetchAvailableSkills(project.id),
      ]);
      setAvailableMcpServers(servers);
      setAvailableSkills(sk);
    } catch {
      // Non-fatal — form still works, just without suggestions
    }
  }, [project?.id]);

  useEffect(() => {
    void loadRoles();
  }, [loadRoles]);

  // Load available MCP servers and skills when entering create/edit mode
  useEffect(() => {
    if (isCreating || editingId) {
      void loadAvailableOptions();
    }
  }, [isCreating, editingId, loadAvailableOptions]);

  const handleCreate = async (data: Omit<CreateAgentRequest, "project_id">) => {
    if (!project) return;
    setCreateBusy(true);
    try {
      const role = await createAgent({ ...data, project_id: project.id });
      setRoles((prev) => [...prev, role]);
      setIsCreating(false);
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to create agent");
    } finally {
      setCreateBusy(false);
    }
  };

  const handleUpdate = async (id: string, data: Omit<CreateAgentRequest, "project_id">) => {
    setEditBusy(true);
    try {
      const updated = await updateAgent(id, {
        name: data.name,
        description: data.description,
        system_prompt_extensions: data.system_prompt_extensions,
        mcp_servers: data.mcp_servers,
        skills: data.skills,
        verification_command: data.verification_command,
      });
      setRoles((prev) => prev.map((r) => (r.id === id ? updated : r)));
      setEditingId(null);
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to update agent");
    } finally {
      setEditBusy(false);
    }
  };

  const handleDelete = async (id: string) => {
    setDeletingId(id);
    try {
      await deleteAgent(id);
      setRoles((prev) => prev.filter((r) => r.id !== id));
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to delete agent");
    } finally {
      setDeletingId(null);
    }
  };

  // Full-page form takeover for create/edit
  const editingRole = editingId ? roles.find((r) => r.id === editingId) : null;

  if (isCreating) {
    return (
      <div className="flex h-full flex-col rounded-lg border border-border bg-card overflow-hidden">
        <AgentForm
          submitLabel="Create"
          isBusy={createBusy}
          availableMcpServers={availableMcpServers}
          availableSkills={availableSkills}
          onSubmit={(data) => void handleCreate(data)}
          onCancel={() => setIsCreating(false)}
        />
      </div>
    );
  }

  if (editingRole) {
    return (
      <div className="flex h-full flex-col rounded-lg border border-border bg-card overflow-hidden">
        <AgentForm
          initial={{
            base_role: editingRole.base_role,
            name: editingRole.name,
            description: editingRole.description,
            system_prompt_extensions: editingRole.system_prompt_extensions,
            mcp_servers: editingRole.mcp_servers,
            skills: editingRole.skills,
            verification_command: editingRole.verification_command,
          }}
          fixedBaseRole={editingRole.base_role}
          submitLabel="Save"
          isBusy={editBusy}
          availableMcpServers={availableMcpServers}
          availableSkills={availableSkills}
          onSubmit={(data) => void handleUpdate(editingRole.id, data)}
          onCancel={() => setEditingId(null)}
        />
      </div>
    );
  }

  if (loading) {
    return (
      <div className="rounded-lg border border-border bg-card p-6 text-sm text-muted-foreground">
        Loading roles...
      </div>
    );
  }

  return (
    <div className="space-y-6">
      <div className="flex items-center justify-between shrink-0">
        <div>
          <h2 className="text-lg font-semibold">Agent Roles</h2>
          <p className="text-sm text-muted-foreground">
            Manage specialist roles that extend base agent behaviour.
          </p>
        </div>
        {isAdmin && (
          <Button onClick={() => setIsCreating(true)}>New Specialist</Button>
        )}
      </div>

      {error && <InlineError message={error} onRetry={() => void loadRoles()} />}

      {roles.length === 0 && !isCreating ? (
        <div className="rounded-lg border border-dashed border-border p-8 text-center text-sm text-muted-foreground">
          No roles configured yet. Create a specialist to extend a base role.
        </div>
      ) : (
        <div className="grid grid-cols-5 gap-3">
          {roles.map((role) => (
            <AgentCard
              key={role.id}
              role={role}
              onEdit={() => setEditingId(role.id)}
              onDelete={() => void handleDelete(role.id)}
              isDeleting={deletingId === role.id}
              canEdit={isAdmin}
            />
          ))}
        </div>
      )}
    </div>
  );
}
