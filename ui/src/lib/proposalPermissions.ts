import type { User } from "@/api/auth";

/** Proposal capabilities derived from the authenticated user's role + admin. */
export interface Caps {
  userId: string;
  role: string;
  isAdmin: boolean;
}

export function capsFromUser(user: User | null): Caps | null {
  if (!user) return null;
  return { userId: user.id, role: user.role, isAdmin: user.isAdmin };
}

/** scoped = PM/engineer/admin; technical = engineer/admin. */
export function canSignoff(c: Caps | null, kind: "scoped" | "technical"): boolean {
  if (!c) return false;
  if (c.isAdmin) return true;
  return kind === "scoped" ? c.role === "pm" || c.role === "engineer" : c.role === "engineer";
}

/** Direct spec edits / accepting suggestions: author, PM, engineer, or admin. */
export function canEdit(c: Caps | null, isAuthor: boolean): boolean {
  if (!c) return false;
  return c.isAdmin || isAuthor || c.role === "pm" || c.role === "engineer";
}

/** Kick off a build (graduate): engineers and admins. */
export function canKickoff(c: Caps | null): boolean {
  if (!c) return false;
  return c.isAdmin || c.role === "engineer";
}
