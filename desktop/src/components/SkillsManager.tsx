import { useCallback, useEffect, useState } from "react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Textarea } from "@/components/ui/textarea";
import { ConfirmButton } from "@/components/ConfirmButton";
import { InlineError } from "@/components/InlineError";
import { useSelectedProject } from "@/stores/useProjectStore";
import {
  type Skill,
  fetchSkills,
  createSkill,
  updateSkill,
  deleteSkill,
} from "@/api/projectTools";

// ── Skill Form ───────────────────────────────────────────────────────────────

interface SkillFormProps {
  initial?: Skill;
  submitLabel: string;
  isBusy: boolean;
  onSubmit: (data: { name: string; description?: string; content: string }) => void;
  onCancel: () => void;
}

function SkillForm({ initial, submitLabel, isBusy, onSubmit, onCancel }: SkillFormProps) {
  const [name, setName] = useState(initial?.name ?? "");
  const [description, setDescription] = useState(initial?.description ?? "");
  const [content, setContent] = useState(initial?.content ?? "");

  const handleSubmit = (e: React.FormEvent) => {
    e.preventDefault();
    onSubmit({
      name: name.trim(),
      description: description.trim() || undefined,
      content: content.trim(),
    });
  };

  return (
    <form onSubmit={handleSubmit} className="flex min-h-0 flex-1 flex-col">
      <div className="shrink-0 space-y-4">
        <div className="flex items-center justify-between">
          <h3 className="text-lg font-semibold text-foreground">
            {initial ? `Edit "${initial.name}"` : "New Skill"}
          </h3>
          <div className="flex gap-2">
            <Button type="button" variant="outline" size="sm" onClick={onCancel} disabled={isBusy}>
              Cancel
            </Button>
            <Button type="submit" size="sm" disabled={isBusy || !name.trim() || !content.trim()}>
              {isBusy ? "Saving..." : submitLabel}
            </Button>
          </div>
        </div>

        <div className="flex flex-wrap gap-4">
          <div className="space-y-1.5 flex-1 min-w-48">
            <Label htmlFor="skill-name" className="text-xs text-muted-foreground">Name</Label>
            <Input
              id="skill-name"
              autoFocus
              placeholder="e.g. rust-safety"
              value={name}
              onChange={(e) => setName(e.target.value)}
              disabled={!!initial}
              required
            />
          </div>
          <div className="space-y-1.5 flex-1 min-w-48">
            <Label htmlFor="skill-description" className="text-xs text-muted-foreground">Description</Label>
            <Input
              id="skill-description"
              placeholder="Short description of what this skill provides"
              value={description}
              onChange={(e) => setDescription(e.target.value)}
            />
          </div>
        </div>
      </div>

      <div className="flex-1 min-h-0 flex flex-col mt-4">
        <Label htmlFor="skill-content" className="text-xs text-muted-foreground mb-2 block shrink-0">
          Skill content (markdown)
        </Label>
        <Textarea
          id="skill-content"
          placeholder={"Instructions, guidelines, or context that will be injected into the agent's system prompt when this skill is active.\n\nSupports full markdown."}
          value={content}
          onChange={(e) => setContent(e.target.value)}
          className="font-mono text-sm flex-1 min-h-[200px] resize-none"
        />
      </div>
    </form>
  );
}

// ── Skill Card ───────────────────────────────────────────────────────────────

function SkillCard({
  skill,
  onEdit,
  onDelete,
  isDeleting,
}: {
  skill: Skill;
  onEdit: () => void;
  onDelete: () => void;
  isDeleting: boolean;
}) {
  const [expanded, setExpanded] = useState(false);

  return (
    <div className="rounded-lg border border-border bg-card p-4 space-y-2">
      <div className="flex items-start justify-between gap-3">
        <div className="min-w-0">
          <div className="flex items-center gap-2">
            <button
              type="button"
              onClick={() => setExpanded((v) => !v)}
              className="font-medium hover:text-primary transition-colors text-left"
            >
              {skill.name}
              <span className="ml-1.5 text-muted-foreground/60 text-xs">
                {expanded ? "▴" : "▾"}
              </span>
            </button>
          </div>
          {skill.description && (
            <p className="text-xs text-muted-foreground mt-0.5">{skill.description}</p>
          )}
        </div>
        <div className="flex items-center gap-2 shrink-0">
          <Button variant="outline" size="sm" onClick={onEdit}>
            Edit
          </Button>
          <ConfirmButton
            title="Delete skill"
            description={`Delete "${skill.name}"? This removes the .md file from .djinn/skills/.`}
            confirmLabel="Delete"
            onConfirm={onDelete}
            size="sm"
            disabled={isDeleting}
          >
            {isDeleting ? "Deleting..." : "Delete"}
          </ConfirmButton>
        </div>
      </div>

      {expanded && (
        <div className="rounded-md bg-muted px-3 py-2 text-xs font-mono text-muted-foreground whitespace-pre-wrap max-h-64 overflow-y-auto">
          {skill.content}
        </div>
      )}
    </div>
  );
}

// ── Main Component ───────────────────────────────────────────────────────────

export function SkillsManager() {
  const project = useSelectedProject();
  const [skills, setSkills] = useState<Skill[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  const [isCreating, setIsCreating] = useState(false);
  const [createBusy, setCreateBusy] = useState(false);
  const [editingName, setEditingName] = useState<string | null>(null);
  const [editBusy, setEditBusy] = useState(false);
  const [deletingName, setDeletingName] = useState<string | null>(null);

  const loadSkills = useCallback(async () => {
    if (!project?.id) return;
    setLoading(true);
    setError(null);
    try {
      setSkills(await fetchSkills(project.id));
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to load skills");
    } finally {
      setLoading(false);
    }
  }, [project?.id]);

  useEffect(() => {
    void loadSkills();
  }, [loadSkills]);

  if (!project) {
    return (
      <div className="rounded-lg border border-dashed border-border p-8 text-center text-sm text-muted-foreground">
        Select a project to manage skills.
      </div>
    );
  }

  const handleCreate = async (data: { name: string; description?: string; content: string }) => {
    setCreateBusy(true);
    try {
      const skill = await createSkill({ ...data, project_id: project.id });
      setSkills((prev) => [...prev, skill].sort((a, b) => a.name.localeCompare(b.name)));
      setIsCreating(false);
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to create skill");
    } finally {
      setCreateBusy(false);
    }
  };

  const handleUpdate = async (data: { name: string; description?: string; content: string }) => {
    setEditBusy(true);
    try {
      const updated = await updateSkill({ ...data, project_id: project.id });
      setSkills((prev) => prev.map((s) => (s.name === updated.name ? updated : s)));
      setEditingName(null);
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to update skill");
    } finally {
      setEditBusy(false);
    }
  };

  const handleDelete = async (name: string) => {
    setDeletingName(name);
    try {
      await deleteSkill(project.id, name);
      setSkills((prev) => prev.filter((s) => s.name !== name));
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to delete skill");
    } finally {
      setDeletingName(null);
    }
  };

  if (isCreating) {
    return (
      <div className="rounded-lg border border-border bg-card p-6">
        <SkillForm
          submitLabel="Create"
          isBusy={createBusy}
          onSubmit={(data) => void handleCreate(data)}
          onCancel={() => setIsCreating(false)}
        />
      </div>
    );
  }

  const editingSkill = editingName ? skills.find((s) => s.name === editingName) : null;
  if (editingSkill) {
    return (
      <div className="rounded-lg border border-border bg-card p-6">
        <SkillForm
          initial={editingSkill}
          submitLabel="Save"
          isBusy={editBusy}
          onSubmit={(data) => void handleUpdate(data)}
          onCancel={() => setEditingName(null)}
        />
      </div>
    );
  }

  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between">
        <div>
          <h3 className="text-xl font-bold">Skills</h3>
          <p className="text-sm text-muted-foreground">
            Prompt-based skills stored in .djinn/skills/. Assign skills to agents to inject domain knowledge into their system prompt.
          </p>
        </div>
        <Button onClick={() => setIsCreating(true)}>New Skill</Button>
      </div>

      {error && <InlineError message={error} onRetry={() => void loadSkills()} />}

      {loading ? (
        <div className="rounded-lg border border-border bg-card p-6 text-sm text-muted-foreground">
          Loading...
        </div>
      ) : skills.length === 0 ? (
        <div className="rounded-lg border border-dashed border-border p-8 text-center text-sm text-muted-foreground">
          No skills yet. Create a skill to provide reusable domain knowledge to your agents.
        </div>
      ) : (
        <div className="space-y-2">
          {skills.map((skill) => (
            <SkillCard
              key={skill.name}
              skill={skill}
              onEdit={() => setEditingName(skill.name)}
              onDelete={() => void handleDelete(skill.name)}
              isDeleting={deletingName === skill.name}
            />
          ))}
        </div>
      )}
    </div>
  );
}
