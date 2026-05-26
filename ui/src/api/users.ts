import { getServerBaseUrl } from "@/api/serverUrl";

/** A member of the deployment's GitHub org, as returned by `GET /users`. */
export interface OrgUser {
  id: string;
  github_login: string;
  github_name: string | null;
  github_avatar_url: string | null;
  is_member_of_org: boolean;
}

export async function fetchUsers(): Promise<OrgUser[]> {
  const baseUrl = getServerBaseUrl();
  const response = await fetch(`${baseUrl}/users`);
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
