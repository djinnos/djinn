import { getServerBaseUrl } from "@/api/serverUrl";

/** A member of the deployment's GitHub org, as returned by `GET /api/users`. */
export interface OrgUser {
  id: string;
  github_login: string;
  github_name: string | null;
  github_avatar_url: string | null;
  is_member_of_org: boolean;
  is_admin: boolean;
  /**
   * True for the non-human "automation" service user — the synthetic account
   * that owns system-initiated work (board patrols, etc.). It can't log in, so
   * an admin configures its credentials + model selection on its behalf via the
   * Configure-automation panel on the Users page.
   */
  is_service?: boolean;
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
