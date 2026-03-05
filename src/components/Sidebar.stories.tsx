import { CalendarDays, LayoutGrid, Settings } from 'lucide-react';
import { Sidebar, type SidebarNavItem } from './Sidebar';

const mockNavItems: SidebarNavItem[] = [
  { id: 'kanban', label: 'Kanban', icon: <LayoutGrid className="h-4 w-4" />, onClick: () => {} },
  { id: 'roadmap', label: 'Roadmap', icon: <CalendarDays className="h-4 w-4" />, onClick: () => {} },
  { id: 'settings', label: 'Settings', icon: <Settings className="h-4 w-4" />, onClick: () => {} },
];

export default {
  title: 'Components/Sidebar',
  component: Sidebar,
  args: {
    isCollapsed: false,
    activeSection: 'kanban',
    navItems: mockNavItems,
    onToggleCollapse: () => {},
  },
};

export const Expanded = {};
export const Collapsed = { args: { isCollapsed: true } };
