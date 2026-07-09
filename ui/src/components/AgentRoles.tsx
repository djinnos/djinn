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

  const handleUpdate = async (
    role: Agent,
    data: Omit<CreateAgentRequest, "project_id">,
  ) => {
    setEditBusy(true);
    try {
      const safeConfiguration = {
        system_prompt_extensions: data.system_prompt_extensions,
        mcp_servers: data.mcp_servers,
        skills: data.skills,
      };
      const updated = await updateAgent(
        role.id,
        role.is_default
          ? safeConfiguration
          : {
              name: data.name,
              description: data.description,
              ...safeConfiguration,
            },
      );
      setRoles((prev) => prev.map((r) => (r.id === role.id ? updated : r)));
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
  const projectDefaults = roles.filter((role) => role.is_default);
  const specialists = roles.filter((role) => !role.is_default);

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
          }}
          fixedBaseRole={editingRole.base_role}
          isDefaultEdit={editingRole.is_default}
          submitLabel="Save"
          isBusy={editBusy}
          availableMcpServers={availableMcpServers}
          availableSkills={availableSkills}
          onSubmit={(data) => void handleUpdate(editingRole, data)}
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
            Customize project-default agents used automatically by Djinn, or create
            task-routed specialists for targeted work.
          </p>
        </div>
      </div>

      {error && <InlineError message={error} onRetry={() => void loadRoles()} />}

      <section aria-labelledby="project-defaults-heading" className="space-y-3">
        <div className="space-y-1">
          <h3 id="project-defaults-heading" className="text-base font-semibold">
            Project defaults
          </h3>
          <p className="text-sm text-muted-foreground">
            These agents are used automatically for worker, planner, lead, reviewer,
            and architect dispatch. Edit their instructions to customize the default
            behavior for this project.
          </p>
        </div>

        {projectDefaults.length === 0 ? (
          <div className="rounded-lg border border-dashed border-border p-6 text-sm text-muted-foreground">
            No project-default agents are available yet.
          </div>
        ) : (
          <div className="grid grid-cols-5 gap-3">
            {projectDefaults.map((role) => (
              <AgentCard
                key={role.id}
                role={role}
                onEdit={() => setEditingId(role.id)}
                onDelete={() => void handleDelete(role.id)}
                isDeleting={deletingId === role.id}
                canEdit={isAdmin}
                editLabel="Edit instructions"
              />
            ))}
          </div>
        )}
      </section>

      <section aria-labelledby="specialists-heading" className="space-y-3">
        <div className="flex items-start justify-between gap-4">
          <div className="space-y-1">
            <h3 id="specialists-heading" className="text-base font-semibold">
              Specialists
            </h3>
            <p className="text-sm text-muted-foreground">
              Specialists run only when a task routes to that specialist agent type
              or name. New Specialist creates specialist-only agents; use Project
              defaults above to customize automatic dispatch.
            </p>
          </div>
          {isAdmin && (
            <Button className="shrink-0" onClick={() => setIsCreating(true)}>
              New Specialist
            </Button>
          )}
        </div>

        {specialists.length === 0 ? (
          <div className="rounded-lg border border-dashed border-border p-6 text-sm text-muted-foreground">
            No specialists configured yet. Create a specialist only when tasks should
            explicitly route to a custom agent type or name; this does not edit
            project defaults.
          </div>
        ) : (
          <div className="grid grid-cols-5 gap-3">
            {specialists.map((role) => (
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
      </section>
    </div>
  );
}
