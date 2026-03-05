import { useMemo } from 'react';
import { useSidebarStore } from '@/stores/sidebarStore';
import { useLocation, useNavigate } from 'react-router-dom';
import { CalendarDays, LayoutGrid, Settings } from 'lucide-react';
import type { SidebarNavItem } from '@/components/Sidebar';

export function useSidebar() {
  const { isCollapsed, toggleCollapse } = useSidebarStore();
  const navigate = useNavigate();
  const location = useLocation();

  const activeSection = useMemo<SidebarNavItem['id']>(() => {
    if (location.pathname.startsWith('/roadmap')) return 'roadmap';
    if (location.pathname.startsWith('/settings')) return 'settings';
    return 'kanban';
  }, [location.pathname]);

  const navItems = useMemo<SidebarNavItem[]>(
    () => [
      {
        id: 'kanban',
        label: 'Kanban',
        icon: <LayoutGrid className="h-4 w-4" />,
        onClick: () => navigate('/'),
      },
      {
        id: 'roadmap',
        label: 'Roadmap',
        icon: <CalendarDays className="h-4 w-4" />,
        onClick: () => navigate('/roadmap'),
      },
      {
        id: 'settings',
        label: 'Settings',
        icon: <Settings className="h-4 w-4" />,
        onClick: () => navigate('/settings'),
      },
    ],
    [navigate]
  );

  return {
    isCollapsed,
    activeSection,
    navItems,
    onToggleCollapse: toggleCollapse,
  };
}
