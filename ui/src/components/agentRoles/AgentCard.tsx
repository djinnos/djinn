import { Button } from "@/components/ui/button";
import { ConfirmButton } from "@/components/ConfirmButton";
import { cn } from "@/lib/utils";
import { getAgentIdentity } from "@/lib/agentIdentity";
import type { Agent } from "@/api/agents";
import { Delete02Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";

export interface AgentCardProps {
  role: Agent;
  onEdit: () => void;
  onDelete: () => void;
  isDeleting: boolean;
  /** Editing agents (incl. their MCP servers + skills) is admin-only. */
  canEdit: boolean;
  editLabel?: string;
}

export function AgentCard({
  role,
  onEdit,
  onDelete,
  isDeleting,
  canEdit,
  editLabel,
}: AgentCardProps) {
  const identity = getAgentIdentity(role.base_role);
  const mcpCount = role.mcp_servers?.length ?? 0;
  const skillCount = role.skills?.length ?? 0;
  const extCount = role.system_prompt_extensions.length;
  const resolvedEditLabel = editLabel ?? (role.is_default ? "Edit instructions" : "Edit");

  return (
    <div className="group relative flex flex-col rounded-xl border border-border bg-card overflow-hidden transition-colors hover:border-border/80">
      {/* Avatar area */}
      <div className="relative flex items-end justify-center bg-muted/30 pt-6 h-36">
        <img
          src={identity.avatar}
          alt={role.base_role}
          className="h-32 w-32 pointer-events-none"
        />
        {/* Hover actions — admin-only */}
        {canEdit && (
          <div className="absolute top-2 right-2 flex gap-1 opacity-0 group-hover:opacity-100 transition-opacity">
            <Button variant="outline" size="sm" className="h-7 px-2 text-xs bg-card" onClick={onEdit}>
              {resolvedEditLabel}
            </Button>
            {!role.is_default && (
              <ConfirmButton
                title="Delete specialist"
                description={`Delete "${role.name}"? This cannot be undone.`}
                confirmLabel="Delete"
                onConfirm={onDelete}
                size="sm"
                disabled={isDeleting}
              >
                <HugeiconsIcon icon={Delete02Icon} size={14} />
              </ConfirmButton>
            )}
          </div>
        )}
      </div>

      {/* Info area */}
      <div className="flex flex-col flex-1 px-4 py-3 space-y-1.5">
        <div className="flex items-center gap-2">
          {!role.is_default && (
            <span className="font-medium text-sm truncate">{role.name}</span>
          )}
          <span className={cn("text-[11px] font-medium", identity.color)}>
            {identity.label}
          </span>
          {role.is_default && (
            <span className="shrink-0 rounded-full bg-muted px-1.5 py-0.5 text-[10px] text-muted-foreground">
              default
            </span>
          )}
        </div>
        {role.description && (
          <p className="text-xs text-muted-foreground line-clamp-2">{role.description}</p>
        )}

        {/* Compact counts */}
        {(extCount > 0 || mcpCount > 0 || skillCount > 0) && (
          <div className="flex flex-wrap gap-1.5 pt-1">
            {extCount > 0 && (
              <span className="rounded-full bg-muted px-1.5 py-0.5 text-[10px] text-muted-foreground">
                {extCount} ext{extCount !== 1 ? "s" : ""}
              </span>
            )}
            {mcpCount > 0 && (
              <span className="rounded-full bg-blue-500/10 px-1.5 py-0.5 text-[10px] text-blue-700 dark:text-blue-300">
                {mcpCount} MCP
              </span>
            )}
            {skillCount > 0 && (
              <span className="rounded-full bg-purple-500/10 px-1.5 py-0.5 text-[10px] text-purple-700 dark:text-purple-300">
                {skillCount} skill{skillCount !== 1 ? "s" : ""}
              </span>
            )}
          </div>
        )}
      </div>
    </div>
  );
}
