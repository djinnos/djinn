import logoSvg from '@/assets/logo.svg';
import { AgentConfig } from '@/components/AgentConfig';
import { Button } from '@/components/ui/button';
import { useAgentConfig } from '@/hooks/settings/useAgentConfig';
import { useModelGateStore } from '@/stores/modelGateStore';
import { useSettingsStore } from '@/stores/settingsStore';

export function ModelOnboarding() {
  const { refresh } = useModelGateStore();
  const agentConfig = useAgentConfig();
  const saveSettings = useSettingsStore((s) => s.saveSettings);

  const handleContinue = async () => {
    const ok = await saveSettings();
    if (ok) {
      await refresh();
    }
  };

  return (
    <main className="flex min-h-screen flex-col items-center justify-center bg-background text-foreground px-6 py-12">
      <div className="flex w-full max-w-2xl flex-col items-center gap-8">

        {/* Logo */}
        <div className="relative">
          <div
            className="pointer-events-none absolute left-1/2 top-1/2 -translate-x-1/2 -translate-y-1/2 h-16 w-16 rounded-full bg-purple-400/40"
            style={{ filter: 'blur(40px)' }}
          />
          <img src={logoSvg} alt="Djinn" className="relative h-16 w-auto drop-shadow-[0_0_40px_rgba(168,139,250,0.35)]" />
        </div>

        {/* Header */}
        <div className="text-center space-y-1">
          <h2 className="text-xl font-semibold">Choose your models</h2>
          <p className="text-sm text-muted-foreground">
            Djinn needs at least one model to run agents and tasks.
          </p>
        </div>

        {/* Model picker */}
        <div className="w-full">
          <AgentConfig
            models={agentConfig.models}
            availableModels={agentConfig.availableModels}
            isLoading={agentConfig.isLoading}
            isSaving={agentConfig.isSaving}
            error={agentConfig.error}
            hasUnsavedChanges={agentConfig.hasUnsavedChanges}
            onAddModel={agentConfig.onAddModel}
            onRemoveModel={agentConfig.onRemoveModel}
            onReorderModels={agentConfig.onReorderModels}
            onUpdateMaxSessions={agentConfig.onUpdateMaxSessions}
            onDismissError={agentConfig.onDismissError}
            onSave={agentConfig.onSave}
            hideHeader
            hideEmptyState
          />
        </div>

        {agentConfig.models.length > 0 && (
          <Button
            size="sm"
            className="px-8"
            disabled={agentConfig.isSaving}
            onClick={() => void handleContinue()}
          >
            {agentConfig.isSaving ? 'Saving...' : 'Continue'}
          </Button>
        )}

      </div>
    </main>
  );
}
