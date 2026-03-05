import { Button } from '@/components/ui/button';
import { cn } from '@/lib/utils';
import { useWizardStore } from '@/stores/wizardStore';
import { NavLink, Navigate, useParams } from 'react-router-dom';

type SettingsCategory = 'providers' | 'projects' | 'general';

const categories: Array<{ key: SettingsCategory; label: string }> = [
  { key: 'providers', label: 'Providers' },
  { key: 'projects', label: 'Projects' },
  { key: 'general', label: 'General' },
];

function ProvidersSettings() {
  return (
    <div className="rounded-lg border border-border bg-card p-6">
      <h2 className="mb-2 text-lg font-semibold">Providers</h2>
      <p className="text-sm text-muted-foreground">
        Configure and manage your provider integrations.
      </p>
    </div>
  );
}

function ProjectsSettings() {
  return (
    <div className="rounded-lg border border-border bg-card p-6">
      <h2 className="mb-2 text-lg font-semibold">Projects</h2>
      <p className="text-sm text-muted-foreground">
        Configure project-specific preferences and defaults.
      </p>
    </div>
  );
}

function GeneralSettings() {
  const { resetWizard } = useWizardStore();

  const handleResetWizard = () => {
    if (confirm('Are you sure you want to reset the wizard? This will show the setup wizard on next launch.')) {
      resetWizard();
    }
  };

  return (
    <div className="space-y-6">
      <div className="rounded-lg border border-border bg-card p-6">
        <h2 className="mb-4 text-lg font-semibold">General</h2>
        <div className="space-y-4">
          <div className="flex items-center justify-between">
            <div>
              <p className="font-medium">Theme</p>
              <p className="text-sm text-muted-foreground">Dark mode is enabled by default</p>
            </div>
            <span className="rounded bg-secondary px-2 py-1 text-xs">Dark</span>
          </div>
        </div>
      </div>

      <div className="rounded-lg border border-border bg-card p-6">
        <h2 className="mb-4 text-lg font-semibold">Setup</h2>
        <div className="space-y-4">
          <div className="flex items-center justify-between">
            <div>
              <p className="font-medium">Setup Wizard</p>
              <p className="text-sm text-muted-foreground">Reset the setup wizard to show on next launch</p>
            </div>
            <Button variant="outline" size="sm" onClick={handleResetWizard}>
              Reset Wizard
            </Button>
          </div>
        </div>
      </div>

      <div className="rounded-lg border border-border bg-card p-6">
        <h2 className="mb-4 text-lg font-semibold">About</h2>
        <div className="space-y-2">
          <p className="text-sm">
            <span className="text-muted-foreground">Version:</span>{' '}
            <span className="font-medium">0.1.0</span>
          </p>
          <p className="text-sm">
            <span className="text-muted-foreground">Build:</span>{' '}
            <span className="font-medium">Development</span>
          </p>
        </div>
      </div>
    </div>
  );
}

export function SettingsPage() {
  const params = useParams<{ category?: string }>();
  const category = params.category as SettingsCategory | undefined;

  if (!category) {
    return <Navigate to="/settings/providers" replace />;
  }

  if (!categories.some((item) => item.key === category)) {
    return <Navigate to="/settings/providers" replace />;
  }

  return (
    <div className="flex h-full flex-col p-6">
      <div className="mb-6">
        <h1 className="text-2xl font-bold text-foreground">Settings</h1>
        <p className="mt-1 text-muted-foreground">Configure your workspace preferences</p>
      </div>

      <div className="flex flex-1 flex-col gap-6 md:flex-row">
        <aside className="md:w-56 md:shrink-0">
          <nav className="flex flex-row gap-2 overflow-x-auto md:flex-col md:overflow-visible">
            {categories.map((item) => (
              <NavLink
                key={item.key}
                to={`/settings/${item.key}`}
                className={({ isActive }) =>
                  cn(
                    'rounded-md px-3 py-2 text-sm transition-colors',
                    isActive
                      ? 'bg-primary text-primary-foreground'
                      : 'text-muted-foreground hover:bg-muted hover:text-foreground',
                  )
                }
              >
                {item.label}
              </NavLink>
            ))}
          </nav>
        </aside>

        <section className="min-w-0 flex-1">
          {category === 'providers' && <ProvidersSettings />}
          {category === 'projects' && <ProjectsSettings />}
          {category === 'general' && <GeneralSettings />}
        </section>
      </div>
    </div>
  );
}
