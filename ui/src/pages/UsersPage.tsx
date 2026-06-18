import { useState } from 'react';
import { useQuery, useQueryClient } from '@tanstack/react-query';
import { HugeiconsIcon } from '@hugeicons/react';
import { Settings02Icon, ShieldUserIcon, UserGroupIcon } from '@hugeicons/core-free-icons';
import { usersQueryOptions } from '@/api/queryOptions';
import { setUserRole, userDisplayName, type OrgUser } from '@/api/users';
import { InlineError } from '@/components/InlineError';
import { UserConfigDialog } from '@/components/UserConfigDialog';
import { useAuthUser } from '@/components/AuthGate';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Skeleton } from '@/components/ui/skeleton';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import { showToast } from '@/lib/toast';
import { relativeTime } from '@/components/memory/memoryUtils';

/**
 * Read-only roster of every user the server knows about. Admin-only — the
 * route and nav entry are gated on `useIsAdmin()` in App/Sidebar, and this page
 * itself never mutates anything.
 */
export function UsersPage() {
  const { data: users, isLoading, isError, error, refetch } = useQuery(usersQueryOptions());

  return (
    <div className="flex h-full flex-col overflow-hidden p-6">
      <div className="mb-5 flex items-center gap-3">
        <span className="flex h-9 w-9 items-center justify-center rounded-lg bg-muted text-muted-foreground">
          <HugeiconsIcon icon={UserGroupIcon} size={18} />
        </span>
        <div>
          <h1 className="text-xl font-bold text-foreground">Users</h1>
          <p className="text-sm text-muted-foreground">
            Everyone with access to this Djinn deployment.
          </p>
        </div>
      </div>

      <div className="min-h-0 flex-1 overflow-y-auto pb-6">
        {isLoading ? (
          <div className="space-y-2">
            {Array.from({ length: 4 }).map((_, i) => (
              <Skeleton key={i} className="h-16 w-full rounded-lg" />
            ))}
          </div>
        ) : isError ? (
          <InlineError
            message={error instanceof Error ? error.message : 'Failed to load users'}
            onRetry={() => void refetch()}
          />
        ) : !users || users.length === 0 ? (
          <div className="rounded-lg border border-dashed border-border bg-card/50 px-4 py-10 text-center text-sm text-muted-foreground">
            No users yet.
          </div>
        ) : (
          <ul className="space-y-2">
            {users.map((user) => (
              <UserRow key={user.id} user={user} />
            ))}
          </ul>
        )}
      </div>
    </div>
  );
}

function UserRow({ user }: { user: OrgUser }) {
  const displayName = userDisplayName(user);
  const initial = (displayName[0] ?? '?').toUpperCase();
  const [configureOpen, setConfigureOpen] = useState(false);
  // Admins configure others here; they manage their own models/limits on the
  // normal Settings page, so hide Configure on their own row.
  const me = useAuthUser();
  const isSelf = me?.id === user.id;
  const queryClient = useQueryClient();
  const role = user.role ?? 'proposer';
  // Admins manage others' roles — not their own.
  const canManageRole = !!me?.isAdmin && !isSelf;

  const changeRole = async (newRole: string) => {
    const key = ['users', 'list'] as const;
    const prev = queryClient.getQueryData<OrgUser[]>(key);
    // Optimistic: reflect immediately so the Select doesn't snap back.
    queryClient.setQueryData<OrgUser[]>(key, (old) =>
      old?.map((u) => (u.id === user.id ? { ...u, role: newRole } : u)),
    );
    try {
      await setUserRole(user.id, newRole);
    } catch (err) {
      queryClient.setQueryData(key, prev);
      showToast.error('Failed to set role', { description: (err as Error).message });
    }
  };

  return (
    <li className="flex items-center gap-3 rounded-lg border border-border bg-card px-4 py-3">
      {user.github_avatar_url ? (
        <img
          src={user.github_avatar_url}
          alt=""
          className="h-9 w-9 shrink-0 rounded-full"
        />
      ) : (
        <div className="flex h-9 w-9 shrink-0 items-center justify-center rounded-full bg-muted text-sm font-medium">
          {initial}
        </div>
      )}

      <div className="min-w-0 flex-1">
        <div className="flex items-center gap-2">
          <span className="truncate text-sm font-medium text-foreground">{displayName}</span>
          {user.is_admin && (
            <Badge variant="secondary" className="gap-1">
              <HugeiconsIcon icon={ShieldUserIcon} size={12} />
              Admin
            </Badge>
          )}
          {user.is_member_of_org && (
            <Badge variant="outline">Org member</Badge>
          )}
          {!user.is_admin && <Badge variant="outline" className="capitalize">{role}</Badge>}
        </div>
        <p className="truncate text-xs text-muted-foreground">@{user.github_login}</p>
      </div>

      {canManageRole && (
        <Select value={role} onValueChange={(v) => typeof v === 'string' && changeRole(v)}>
          <SelectTrigger className="h-8 w-[130px] shrink-0 text-xs" title="Proposal role">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            <SelectItem value="proposer">Proposer</SelectItem>
            <SelectItem value="pm">PM</SelectItem>
            <SelectItem value="engineer">Engineer</SelectItem>
          </SelectContent>
        </Select>
      )}

      {user.last_seen_at && (
        <span className="shrink-0 text-xs text-muted-foreground" title={user.last_seen_at}>
          Last seen {relativeTime(user.last_seen_at)}
        </span>
      )}

      {!isSelf && (
        <>
          <Button
            variant="outline"
            size="sm"
            className="shrink-0"
            onClick={() => setConfigureOpen(true)}
          >
            <HugeiconsIcon icon={Settings02Icon} size={14} />
            Configure
          </Button>
          <UserConfigDialog
            user={user}
            open={configureOpen}
            onOpenChange={setConfigureOpen}
          />
        </>
      )}
    </li>
  );
}
