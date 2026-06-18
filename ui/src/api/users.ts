import { getServerBaseUrl } from "@/api/serverUrl";

/** A member of the deployment's GitHub org, as returned by `GET /api/users`. */
export interface OrgUser {
  id: string;
  github_login: string;
  github_name: string | null;
  github_avatar_url: string | null;
  is_member_of_org: boolean;
  is_admin: boolean;
  /** Proposal capability role: `proposer` | `pm` | `engineer`. */
  role?: string;
  /** Optional last-seen timestamp (ISO 8601), surfaced when the server sends it. */
  last_seen_at?: string | null;
}

export async function fetchUsers(): Promise<OrgUser[]> {
  const baseUrl = getServerBaseUrl();
  const response = await fetch(`${baseUrl}/api/users`);
  if (!response.ok) {
    throw new Error(`Failed to fetch users: ${response.status}`);
  }
  const data = (await response.json()) as { users: OrgUser[] };
  return data.users;
}

/** Human-friendly label for a user: real name when known, else the login. */
export function userDisplayName(user: OrgUser): string {
  return user.github_name?.trim() || user.github_login;
}

/** Admin-only: set a user's proposal capability role. */
export async function setUserRole(userId: string, role: string): Promise<void> {
  const baseUrl = getServerBaseUrl();
  const response = await fetch(`${baseUrl}/api/users/${userId}/role`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    credentials: "include",
    body: JSON.stringify({ role }),
  });
  if (!response.ok) {
    throw new Error(`Failed to set role: ${response.status}`);
  }
}
