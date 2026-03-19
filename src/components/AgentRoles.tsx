import { useCallback, useEffect, useState } from "react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Textarea } from "@/components/ui/textarea";
import { ConfirmButton } from "@/components/ConfirmButton";
import { InlineError } from "@/components/InlineError";
import { cn } from "@/lib/utils";
import {
  type BaseRole,
  type CreateRoleRequest,
  type Role,
  createRole,
  deleteRole,
  fetchRoles,
  updateRole,
} from "@/api/roles";

const BASE_ROLE_LABELS: Record<BaseRole, string> = {
  worker: "Worker",
  task_reviewer: "Task Reviewer",
  pm: "Planner (PM)",
  groomer: "Groomer",
};

const BASE_ROLES: BaseRole[] = ["worker", "task_reviewer", "pm", "groomer"];

// ── Role Form ────────────────────────────────────────────────────────────────

interface RoleFormProps {
  initial?: Partial<CreateRoleRequest>;
  fixedBaseRole?: BaseRole;
  submitLabel: string;
  isBusy: boolean;
  onSubmit: (data: CreateRoleRequest) => void;
  onCancel: () => void;
}

function RoleForm({ initial, fixedBaseRole, submitLabel, isBusy, onSubmit, onCancel }: RoleFormProps) {
  const [baseRole, setBaseRole] = useState<BaseRole>(fixedBaseRole ?? initial?.base_role ?? "worker");
  const [name, setName] = useState(initial?.name ?? "");
  const [description, setDescription] = useState(initial?.description ?? "");
  const [extensions, setExtensions] = useState(
    (initial?.system_prompt_extensions ?? []).join("\n"),
  );

  const handleSubmit = (e: React.FormEvent) => {
    e.preventDefault();
    onSubmit({
      base_role: baseRole,
      name: name.trim(),
      description: description.trim(),
      system_prompt_extensions: extensions
        .split("\n")
        .map((line) => line.trim())
        .filter(Boolean),
    });
  };

  return (
    <form onSubmit={handleSubmit} className="space-y-4">
      {!fixedBaseRole && (
        <div className="space-y-1.5">
          <Label>Base role</Label>
          <div className="flex flex-wrap gap-2">
            {BASE_ROLES.map((role) => (
              <button
                key={role}
                type="button"
                onClick={() => setBaseRole(role)}
                className={cn(
                  "rounded-md border px-3 py-1.5 text-sm transition-colors",
                  baseRole === role
                    ? "border-primary bg-primary text-primary-foreground"
                    : "border-border bg-card text-muted-foreground hover:bg-muted",
                )}
              >
                {BASE_ROLE_LABELS[role]}
              </button>
            ))}
          </div>
        </div>
      )}

      <div className="space-y-1.5">
        <Label htmlFor="role-name">Name</Label>
        <Input
          id="role-name"
          autoFocus
          placeholder="e.g. Senior Backend Worker"
          value={name}
          onChange={(e) => setName(e.target.value)}
          required
        />
      </div>

      <div className="space-y-1.5">
        <Label htmlFor="role-description">Description</Label>
        <Input
          id="role-description"
          placeholder="Short description of what this specialist does"
          value={description}
          onChange={(e) => setDescription(e.target.value)}
        />
      </div>

      <div className="space-y-1.5">
        <Label htmlFor="role-extensions">
          Prompt extensions{" "}
          <span className="text-muted-foreground font-normal">(one per line)</span>
        </Label>
        <Textarea
          id="role-extensions"
          placeholder={"You specialise in Rust systems programming.\nAlways write safe, idiomatic code."}
          value={extensions}
          onChange={(e) => setExtensions(e.target.value)}
          rows={4}
          className="font-mono text-sm resize-y"
        />
      </div>

      <div className="flex gap-2 justify-end pt-1">
        <Button type="button" variant="outline" onClick={onCancel} disabled={isBusy}>
          Cancel
        </Button>
        <Button type="submit" disabled={isBusy || !name.trim()}>
          {isBusy ? "Saving..." : submitLabel}
        </Button>
      </div>
    </form>
  );
}

// ── Role Card ────────────────────────────────────────────────────────────────

interface RoleCardProps {
  role: Role;
  onEdit: () => void;
  onDelete: () => void;
  isDeleting: boolean;
}

function RoleCard({ role, onEdit, onDelete, isDeleting }: RoleCardProps) {
  return (
    <div className="rounded-lg border border-border bg-card p-4 space-y-2">
      <div className="flex items-start justify-between gap-3">
        <div className="min-w-0">
          <div className="flex items-center gap-2">
            <span className="font-medium truncate">{role.name}</span>
            {role.is_default && (
              <span className="shrink-0 rounded-full bg-muted px-2 py-0.5 text-xs text-muted-foreground">
                default
              </span>
            )}
          </div>
          {role.description && (
            <p className="text-xs text-muted-foreground mt-0.5 truncate">{role.description}</p>
          )}
        </div>

        {!role.is_default && (
          <div className="flex items-center gap-2 shrink-0">
            <Button variant="outline" size="sm" onClick={onEdit}>
              Edit
            </Button>
            <ConfirmButton
              title="Delete specialist"
              description={`Delete "${role.name}"? This cannot be undone.`}
              confirmLabel="Delete"
              onConfirm={onDelete}
              size="sm"
              disabled={isDeleting}
            >
              {isDeleting ? "Deleting..." : "Delete"}
            </ConfirmButton>
          </div>
        )}
      </div>

      {role.system_prompt_extensions.length > 0 && (
        <div className="rounded-md bg-muted px-3 py-2 text-xs font-mono text-muted-foreground space-y-0.5">
          {role.system_prompt_extensions.map((ext, i) => (
            <p key={i}>{ext}</p>
          ))}
        </div>
      )}
    </div>
  );
}

// ── Main Component ───────────────────────────────────────────────────────────

export function AgentRoles() {
  const [roles, setRoles] = useState<Role[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

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
      const data = await fetchRoles();
      setRoles(data);
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to load roles");
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void loadRoles();
  }, [loadRoles]);

  const handleCreate = async (data: CreateRoleRequest) => {
    setCreateBusy(true);
    try {
      const role = await createRole(data);
      setRoles((prev) => [...prev, role]);
      setIsCreating(false);
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to create role");
    } finally {
      setCreateBusy(false);
    }
  };

  const handleUpdate = async (id: string, data: CreateRoleRequest) => {
    setEditBusy(true);
    try {
      const updated = await updateRole(id, {
        name: data.name,
        description: data.description,
        system_prompt_extensions: data.system_prompt_extensions,
      });
      setRoles((prev) => prev.map((r) => (r.id === id ? updated : r)));
      setEditingId(null);
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to update role");
    } finally {
      setEditBusy(false);
    }
  };

  const handleDelete = async (id: string) => {
    setDeletingId(id);
    try {
      await deleteRole(id);
      setRoles((prev) => prev.filter((r) => r.id !== id));
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to delete role");
    } finally {
      setDeletingId(null);
    }
  };

  // Group roles by base_role
  const grouped = BASE_ROLES.map((baseRole) => ({
    baseRole,
    label: BASE_ROLE_LABELS[baseRole],
    roles: roles.filter((r) => r.base_role === baseRole),
  })).filter((g) => g.roles.length > 0);

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
        {!isCreating && (
          <Button onClick={() => setIsCreating(true)}>New Specialist</Button>
        )}
      </div>

      {error && <InlineError message={error} onRetry={() => void loadRoles()} />}

      {isCreating && (
        <div className="rounded-lg border border-border bg-card p-4">
          <h3 className="font-medium mb-4">New specialist</h3>
          <RoleForm
            submitLabel="Create"
            isBusy={createBusy}
            onSubmit={(data) => void handleCreate(data)}
            onCancel={() => setIsCreating(false)}
          />
        </div>
      )}

      {roles.length === 0 && !isCreating ? (
        <div className="rounded-lg border border-dashed border-border p-8 text-center text-sm text-muted-foreground">
          No roles configured yet. Create a specialist to extend a base role.
        </div>
      ) : (
        <div className="space-y-6">
          {grouped.map(({ baseRole, label, roles: groupRoles }) => (
            <div key={baseRole}>
              <h3 className="text-sm font-semibold text-muted-foreground uppercase tracking-wide mb-2">
                {label}
              </h3>
              <div className="space-y-2">
                {groupRoles.map((role) =>
                  editingId === role.id ? (
                    <div key={role.id} className="rounded-lg border border-border bg-card p-4">
                      <h4 className="font-medium mb-4">Edit "{role.name}"</h4>
                      <RoleForm
                        initial={{
                          base_role: role.base_role,
                          name: role.name,
                          description: role.description,
                          system_prompt_extensions: role.system_prompt_extensions,
                        }}
                        fixedBaseRole={role.base_role}
                        submitLabel="Save"
                        isBusy={editBusy}
                        onSubmit={(data) => void handleUpdate(role.id, data)}
                        onCancel={() => setEditingId(null)}
                      />
                    </div>
                  ) : (
                    <RoleCard
                      key={role.id}
                      role={role}
                      onEdit={() => setEditingId(role.id)}
                      onDelete={() => void handleDelete(role.id)}
                      isDeleting={deletingId === role.id}
                    />
                  ),
                )}
              </div>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}
