import { Button } from '@/components/ui/button';
import { useWizardStore } from '@/stores/wizardStore';

export function SettingsPage() {
  const { resetWizard } = useWizardStore();

  const handleResetWizard = () => {
    if (confirm('Are you sure you want to reset the wizard? This will show the setup wizard on next launch.')) {
      resetWizard();
    }
  };

  return (
    <div className="flex h-full flex-col p-6">
      <div className="mb-6">
        <h1 className="text-2xl font-bold text-foreground">Settings</h1>
        <p className="text-muted-foreground mt-1">
          Configure your workspace preferences
        </p>
      </div>
      
      <div className="max-w-2xl space-y-6">
        {/* General Settings */}
        <div className="rounded-lg border border-border bg-card p-6">
          <h2 className="text-lg font-semibold mb-4">General</h2>
          <div className="space-y-4">
            <div className="flex items-center justify-between">
              <div>
                <p className="font-medium">Theme</p>
                <p className="text-sm text-muted-foreground">Dark mode is enabled by default</p>
              </div>
              <span className="text-xs bg-secondary px-2 py-1 rounded">Dark</span>
            </div>
          </div>
        </div>

        {/* Wizard Settings */}
        <div className="rounded-lg border border-border bg-card p-6">
          <h2 className="text-lg font-semibold mb-4">Setup</h2>
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

        {/* About */}
        <div className="rounded-lg border border-border bg-card p-6">
          <h2 className="text-lg font-semibold mb-4">About</h2>
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
    </div>
  );
}
