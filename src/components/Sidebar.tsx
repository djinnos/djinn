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
import logoSvg from '@/assets/logo.svg';
import { useEffect, useCallback } from 'react';
import { useLocation, useNavigate } from 'react-router-dom';

interface NavItemProps {
  icon: React.ReactNode;
  label: string;
  hotkey?: string;
  isActive: boolean;
  isCollapsed: boolean;
  onClick: () => void;
}

function NavItem({ icon, label, hotkey, isActive, isCollapsed, onClick }: NavItemProps) {
  return (
    <Button
      variant={isActive ? 'secondary' : 'ghost'}
      size={isCollapsed ? 'icon' : 'default'}
      onClick={onClick}
      className={cn(
        'w-full justify-start gap-3 transition-all duration-200',
        isCollapsed ? 'h-10 w-10 justify-center' : 'h-9 px-3',
        isActive && 'bg-white/[0.05] text-foreground'
      )}
      title={isCollapsed ? `${label}${hotkey ? ` (${hotkey.toUpperCase()})` : ''}` : undefined}
    >
      <span className="flex h-4 w-4 items-center justify-center shrink-0">
        {icon}
      </span>
      {!isCollapsed && (
        <>
          <span className="text-sm font-medium truncate flex-1 text-left">{label}</span>
          {hotkey && (
            <kbd className="inline-flex h-4 items-center justify-center rounded border border-sidebar-border px-1 font-mono text-[10px] text-muted-foreground/50">
              {hotkey.toUpperCase()}
            </kbd>
          )}
        </>
      )}
    </Button>
  );
}

export function Sidebar() {
  const { isCollapsed, activeSection, toggleCollapse, setActiveSection } = useSidebarStore();
  const navigate = useNavigate();
  const location = useLocation();

  const handleKeyDown = useCallback((e: KeyboardEvent) => {
    if ((e.metaKey || e.ctrlKey) && e.key === '/') {
      e.preventDefault();
      toggleCollapse();
      return;
    }

    // Skip hotkeys when typing in inputs
    const tag = (e.target as HTMLElement).tagName;
    if (tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT' || (e.target as HTMLElement).isContentEditable) return;
    if (e.metaKey || e.ctrlKey || e.altKey) return;

    switch (e.key.toLowerCase()) {
      case 'k':
        e.preventDefault();
        navigate('/');
        break;
      case 'e':
        e.preventDefault();
        navigate('/roadmap');
        break;
      case 's':
        e.preventDefault();
        navigate('/settings');
        break;
    }
  }, [toggleCollapse, navigate]);

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
      hotkey: 'k',
    },
    {
      id: 'roadmap' as const,
      label: 'Epics',
      icon: <HugeiconsIcon icon={Flag02Icon} size={16} />,
      hotkey: 'e',
    },
  ];

  return (
    <aside
      className={cn(
        'flex h-screen shrink-0 flex-col border-r bg-sidebar transition-all duration-200 ease-in-out',
        isCollapsed ? 'w-14' : 'w-64'
      )}
    >
      {/* Header — px-5 aligns logo with nav icons (nav p-2 + button px-3) */}
      <div data-tauri-drag-region className={cn("flex h-12 items-center border-b", isCollapsed ? "justify-center px-2" : "px-5")}>
        <div className={cn("flex items-center gap-3", !isCollapsed && "flex-1")}>
          <span className="flex h-4 w-4 items-center justify-center shrink-0">
            <img src={logoSvg} alt="Djinn" className="h-4 w-4" />
          </span>
          {!isCollapsed && (
            <span className="text-sm font-semibold text-sidebar-foreground truncate">
              Djinn
            </span>
          )}
        </div>
        {!isCollapsed && (
          <>
            <div className="flex items-center gap-1 text-[10px] text-sidebar-foreground/50">
              <kbd className="inline-flex h-4 items-center justify-center rounded border border-sidebar-border px-1 font-mono">
                <Command className="h-2.5 w-2.5" />
              </kbd>
              <kbd className="inline-flex h-4 items-center justify-center rounded border border-sidebar-border px-1 font-mono">
                /
              </kbd>
            </div>
            <Button
              variant="ghost"
              size="icon"
              onClick={toggleCollapse}
              className="h-8 w-8 shrink-0"
              title="Collapse sidebar (Cmd+/)"
            >
              <PanelLeft className="h-4 w-4 transition-transform duration-200" />
            </Button>
          </>
        )}
      </div>

      {/* Navigation */}
      <nav className="flex-1 overflow-y-auto p-2 space-y-1">
        {navItems.map((item) => (
          <NavItem
            key={item.id}
            icon={item.icon}
            label={item.label}
            hotkey={item.hotkey}
            isActive={activeSection === item.id}
            isCollapsed={isCollapsed}
            onClick={() => navigate(item.id === 'kanban' ? '/' : `/${item.id}`)}
          />
        ))}
        <NavItem
          icon={<Settings className="h-4 w-4" />}
          label="Settings"
          hotkey="s"
          isActive={activeSection === 'settings'}
          isCollapsed={isCollapsed}
          onClick={() => navigate('/settings')}
        />
      </nav>
    </aside>
  );
}
