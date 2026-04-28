import { useSidebarStore } from '@/stores/sidebarStore';
import { useAuthUser } from '@/components/AuthGate';
import { logout } from '@/api/auth';
import { Button } from '@/components/ui/button';
import { cn } from '@/lib/utils';

import {
  KanbanIcon,
  Robot01Icon,
  ChatIcon,
  LogoutSquare01Icon,
  Settings01Icon,
  WorkflowSquare06Icon,
  Brain01Icon,
  Idea01Icon,
  ConnectIcon,
  GithubIcon,
} from '@hugeicons/core-free-icons';
import { HugeiconsIcon } from '@hugeicons/react';
import logoSvg from '@/assets/logo.svg';
import { useEffect } from 'react';
import { useQuery } from '@tanstack/react-query';
import { useLocation, useNavigate } from 'react-router-dom';
import { useProjectRoute } from '@/hooks/useProjectRoute';
import { useDevcontainerWarnings } from '@/hooks/useDevcontainerWarnings';
import { showToast } from '@/lib/toast';
import {
  allProjectsProposalListQueryOptions,
  markProposalDraftNotified,
  shouldNotifyForProposalDraft,
} from '@/lib/pulseProposals';

interface NavItemProps {
  icon: React.ReactNode;
  label: string;
  badgeCount?: number;
  warningCount?: number;
  warningLabel?: string;
  isActive: boolean;
  onClick: () => void;
}

function NavItem({ icon, label, badgeCount, warningCount, warningLabel, isActive, onClick }: NavItemProps) {
  const hasWarning = typeof warningCount === 'number' && warningCount > 0;
  const ariaLabel =
    typeof badgeCount === 'number' && badgeCount > 0
      ? `${label} has ${badgeCount} pending proposals`
      : hasWarning
      ? `${label} — ${warningCount} ${warningLabel ?? 'items need attention'}`
      : undefined;

  return (
    <Button
      variant={isActive ? 'secondary' : 'ghost'}
      size="default"
      onClick={onClick}
      aria-label={ariaLabel}
      className={cn(
        'w-full justify-start gap-3 transition-all duration-200',
        'h-9 px-3',
        isActive && 'bg-white/[0.05] text-foreground'
      )}
    >
      <span className="flex h-4 w-4 items-center justify-center shrink-0">
        {icon}
      </span>
      <span className="text-sm font-medium truncate flex-1 text-left">{label}</span>
      {typeof badgeCount === 'number' && badgeCount > 0 ? (
        <span className="inline-flex min-w-5 items-center justify-center rounded-full bg-primary px-1.5 py-0.5 text-[11px] font-semibold leading-none text-primary-foreground">
          {badgeCount}
        </span>
      ) : hasWarning ? (
        <span
          className="inline-flex min-w-5 items-center justify-center rounded-full bg-amber-500/20 px-1.5 py-0.5 text-[11px] font-semibold leading-none text-amber-300 ring-1 ring-amber-500/40"
          title={warningLabel ?? 'Needs attention'}
        >
          {warningCount}
        </span>
      ) : null}
    </Button>
  );
}

function UserFooter() {
  const user = useAuthUser();

  if (!user) return null;

  const displayName = user.name || user.login;
  const initial = (user.name?.[0] || user.login?.[0] || '?').toUpperCase();

  return (
    <div className="flex items-center gap-2.5 rounded-md px-2 py-2">
      {user.avatarUrl ? (
        <img src={user.avatarUrl} alt="" className="h-7 w-7 shrink-0 rounded-full" />
      ) : (
        <div className="flex h-7 w-7 shrink-0 items-center justify-center rounded-full bg-muted text-xs font-medium">
          {initial}
        </div>
      )}
      <div className="min-w-0 flex-1">
        <p className="truncate text-sm font-medium text-sidebar-foreground">{displayName}</p>
        <p className="truncate text-[11px] text-muted-foreground">@{user.login}</p>
      </div>
      <button
        type="button"
        onClick={() => void logout()}
        className="flex h-6 w-6 shrink-0 items-center justify-center rounded-md text-muted-foreground transition-colors hover:bg-white/10 hover:text-foreground"
        title="Sign out"
      >
        <HugeiconsIcon icon={LogoutSquare01Icon} className="h-3.5 w-3.5" />
      </button>
    </div>
  );
}

export function Sidebar() {
  const { activeSection, setActiveSection } = useSidebarStore();
  const navigate = useNavigate();
  const location = useLocation();
  const { navigateToView } = useProjectRoute();
  const user = useAuthUser();
  const proposalsQuery = useQuery(allProjectsProposalListQueryOptions());
  const proposalCount = proposalsQuery.data?.length ?? 0;
  const { count: devcontainerWarningCount } = useDevcontainerWarnings();

  // Sync active section from URL
  useEffect(() => {
    if (location.pathname.includes('/chat')) {
      setActiveSection('chat');
    } else if (location.pathname.includes('/roadmap')) {
      setActiveSection('roadmap');
    } else if (location.pathname.includes('/agents') || location.pathname.includes('/metrics')) {
      setActiveSection('agents');
    } else if (location.pathname.includes('/memory')) {
      setActiveSection('memory');
    } else if (location.pathname.includes('/code-graph')) {
      setActiveSection('code-graph');
    } else if (location.pathname.includes('/proposals')) {
      setActiveSection('proposals');
    } else if (location.pathname.startsWith('/repositories')) {
      setActiveSection('repositories');
    } else if (location.pathname.startsWith('/settings')) {
      setActiveSection('settings');
    } else {
      setActiveSection('kanban');
    }
  }, [location.pathname, setActiveSection]);

  useEffect(() => {
    for (const proposal of proposalsQuery.data ?? []) {
      if (!shouldNotifyForProposalDraft(proposal, user)) continue;

      markProposalDraftNotified(proposal.id);
      showToast.info('Architect proposal draft is ready', {
        description: proposal.originating_spike_id
          ? `Spike ${proposal.originating_spike_id} produced "${proposal.title || proposal.id}".`
          : `"${proposal.title || proposal.id}" is ready for review.`,
      });
    }
  }, [proposalsQuery.data, user]);

  return (
    <aside className="flex h-screen w-64 shrink-0 flex-col border-r bg-sidebar">
      {/* Header */}
      <div data-drag-region className="flex h-12 items-center border-b px-5">
        <div className="flex flex-1 items-center gap-3">
          <span className="flex h-4 w-4 items-center justify-center shrink-0 overflow-visible">
            <img src={logoSvg} alt="Djinn" className="h-6 w-6" />
          </span>
          <span className="text-sm font-semibold text-sidebar-foreground truncate">
            Djinn
          </span>
        </div>
      </div>

      {/* Navigation */}
      <nav className="flex-1 p-2 space-y-1 overflow-y-auto">
        <NavItem
          icon={<HugeiconsIcon icon={ChatIcon} className="h-4 w-4" />}
          label="Chat"
          isActive={activeSection === 'chat'}
          onClick={() => navigateToView('chat')}
        />
        <NavItem
          icon={<HugeiconsIcon icon={KanbanIcon} className="h-4 w-4" />}
          label="Kanban"
          isActive={activeSection === 'kanban'}
          onClick={() => navigateToView('kanban')}
        />
        <NavItem
          icon={<HugeiconsIcon icon={WorkflowSquare06Icon} className="h-4 w-4" />}
          label="Roadmap"
          isActive={activeSection === 'roadmap'}
          onClick={() => navigateToView('roadmap')}
        />
        <NavItem
          icon={<HugeiconsIcon icon={ConnectIcon} className="h-4 w-4" />}
          label="Code Graph"
          isActive={activeSection === 'code-graph'}
          onClick={() => navigateToView('code-graph')}
        />
        <NavItem
          icon={<HugeiconsIcon icon={Idea01Icon} className="h-4 w-4" />}
          label="Proposals"
          badgeCount={proposalCount}
          isActive={activeSection === 'proposals'}
          onClick={() => navigateToView('proposals')}
        />
        <NavItem
          icon={<HugeiconsIcon icon={Robot01Icon} className="h-4 w-4" />}
          label="Agents"
          isActive={activeSection === 'agents'}
          onClick={() => navigateToView('agents')}
        />
        <NavItem
          icon={<HugeiconsIcon icon={Brain01Icon} className="h-4 w-4" />}
          label="Memory"
          isActive={activeSection === 'memory'}
          onClick={() => navigateToView('memory')}
        />
        <NavItem
          icon={<HugeiconsIcon icon={GithubIcon} className="h-4 w-4" />}
          label="Repositories"
          warningCount={devcontainerWarningCount}
          warningLabel="need devcontainer setup"
          isActive={activeSection === 'repositories'}
          onClick={() => navigate('/repositories')}
        />
      </nav>

      {/* Footer */}
      <div className="border-t p-3 space-y-2">
        <NavItem
          icon={<HugeiconsIcon icon={Settings01Icon} size={16} />}
          label="Settings"
          isActive={activeSection === 'settings'}
          onClick={() => navigate('/settings')}
        />
        <UserFooter />
      </div>
    </aside>
  );
}
