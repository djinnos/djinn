import workerAvatar from "@/assets/worker.png";
import reviewerAvatar from "@/assets/reviewer.png";
import leadAvatar from "@/assets/lead.png";
import plannerAvatar from "@/assets/planner.png";
import architectAvatar from "@/assets/architect.png";

export interface AgentIdentity {
  label: string;
  color: string;
  avatar: string;
}

const AGENTS: Record<string, AgentIdentity> = {
  worker:        { label: "Worker",        color: "text-blue-400",    avatar: workerAvatar },
  reviewer:      { label: "Reviewer",      color: "text-amber-400",   avatar: reviewerAvatar },
  pm:            { label: "Lead",          color: "text-red-400",     avatar: leadAvatar },
  lead:          { label: "Lead",          color: "text-red-400",     avatar: leadAvatar },
  planner:       { label: "Planner",       color: "text-purple-400",  avatar: plannerAvatar },
  architect:     { label: "Architect",     color: "text-emerald-400", avatar: architectAvatar },
  epic_reviewer: { label: "Epic Reviewer", color: "text-teal-400",    avatar: reviewerAvatar },
  system:        { label: "System",        color: "text-zinc-400",    avatar: workerAvatar },
};

const FALLBACK: AgentIdentity = { label: "Agent", color: "text-muted-foreground", avatar: workerAvatar };

export function getAgentIdentity(agentType?: string): AgentIdentity {
  return AGENTS[agentType ?? "worker"] ?? FALLBACK;
}

export function getAgentAvatar(agentType?: string): string {
  return getAgentIdentity(agentType).avatar;
}
