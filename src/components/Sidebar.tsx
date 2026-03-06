import { useSidebarStore } from '@/stores/sidebarStore';
import { Button } from '@/components/ui/button';
import { cn } from '@/lib/utils';
import {
  Settings,
  PanelLeft,
  Command
} from 'lucide-react';
import { Flag02Icon, KanbanIcon } from '@hugeicons/core-free-icons';
import { HugeiconsIcon } from '@hugeicons/react';
import { useEffect, useCallback } from 'react';
import { useLocation, useNavigate } from 'react-router-dom';

interface NavItemProps {
  icon: React.ReactNode;
  label: string;
  isActive: boolean;
  isCollapsed: boolean;
  onClick: () => void;
}

function NavItem({ icon, label, isActive, isCollapsed, onClick }: NavItemProps) {
  return (
    <Button
      variant={isActive ? 'secondary' : 'ghost'}
      size={isCollapsed ? 'icon' : 'default'}
      onClick={onClick}
      className={cn(
        'w-full justify-start gap-3 transition-all duration-200',
        isCollapsed ? 'h-10 w-10 justify-center' : 'h-9 px-3',
        isActive && 'bg-secondary text-secondary-foreground'
      )}
      title={isCollapsed ? label : undefined}
    >
      <span className="flex h-4 w-4 items-center justify-center shrink-0">
        {icon}
      </span>
      {!isCollapsed && (
        <span className="text-sm font-medium truncate">{label}</span>
      )}
    </Button>
  );
}

export function Sidebar() {
  const { isCollapsed, activeSection, toggleCollapse, setActiveSection } = useSidebarStore();
  const navigate = useNavigate();
  const location = useLocation();

  // Keyboard shortcut: Cmd+/ to toggle sidebar
  const handleKeyDown = useCallback((e: KeyboardEvent) => {
    if ((e.metaKey || e.ctrlKey) && e.key === '/') {
      e.preventDefault();
      toggleCollapse();
    }
  }, [toggleCollapse]);

  useEffect(() => {
    window.addEventListener('keydown', handleKeyDown);
    return () => window.removeEventListener('keydown', handleKeyDown);
  }, [handleKeyDown]);

  useEffect(() => {
    if (location.pathname.startsWith('/roadmap')) {
      setActiveSection('roadmap');
    } else if (location.pathname.startsWith('/settings')) {
      setActiveSection('settings');
    } else {
      setActiveSection('kanban');
    }
  }, [location.pathname, setActiveSection]);

  const navItems = [
    {
      id: 'kanban' as const,
      label: 'Kanban',
      icon: <HugeiconsIcon icon={KanbanIcon} size={16} />,
    },
    {
      id: 'roadmap' as const,
      label: 'Epics',
      icon: <HugeiconsIcon icon={Flag02Icon} size={16} />,
    },
  ];

  return (
    <aside
      className={cn(
        'flex flex-col border-r bg-sidebar transition-all duration-200 ease-in-out',
        isCollapsed ? 'w-14' : 'w-64'
      )}
    >
      {/* Header */}
      <div className="flex h-12 items-center border-b px-3">
        {!isCollapsed && (
          <span className="flex-1 text-sm font-semibold text-sidebar-foreground truncate">
            DjinnOS
          </span>
        )}
        <Button
          variant="ghost"
          size="icon"
          onClick={toggleCollapse}
          className={cn(
            'h-8 w-8 shrink-0',
            isCollapsed && 'mx-auto'
          )}
          title={isCollapsed ? 'Expand sidebar (Cmd+/)' : 'Collapse sidebar (Cmd+/)'}
        >
          <PanelLeft className={cn(
            'h-4 w-4 transition-transform duration-200',
            isCollapsed && 'rotate-180'
          )} />
        </Button>
      </div>

      {/* Main Navigation */}
      <nav className="flex-1 overflow-y-auto p-2 space-y-1">
        {navItems.map((item) => (
          <NavItem
            key={item.id}
            icon={item.icon}
            label={item.label}
            isActive={activeSection === item.id}
            isCollapsed={isCollapsed}
            onClick={() => navigate(item.id === 'kanban' ? '/' : `/${item.id}`)}
          />
        ))}
      </nav>

      {/* Bottom Section - Settings */}
      <div className="border-t p-2">
        <NavItem
          icon={<Settings className="h-4 w-4" />}
          label="Settings"
          isActive={activeSection === 'settings'}
          isCollapsed={isCollapsed}
          onClick={() => navigate('/settings')}
        />
        
        {/* Keyboard shortcut hint */}
        {!isCollapsed && (
          <div className="mt-2 flex items-center justify-center gap-1 text-[10px] text-sidebar-foreground/50">
            <kbd className="inline-flex h-4 items-center justify-center rounded border border-sidebar-border px-1 font-mono">
              <Command className="h-2.5 w-2.5" />
            </kbd>
            <span>+</span>
            <kbd className="inline-flex h-4 items-center justify-center rounded border border-sidebar-border px-1 font-mono text-[10px]">
              /
            </kbd>
          </div>
        )}
      </div>
    </aside>
  );
}
