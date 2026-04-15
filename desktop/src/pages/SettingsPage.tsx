import { useState } from 'react';
import { Delete02Icon } from '@hugeicons/core-free-icons';
import { HugeiconsIcon } from '@hugeicons/react';
import { Button } from '@/components/ui/button';
import { InlineError } from '@/components/InlineError';
import { EmptyState } from '@/components/EmptyState';
import { AgentConfig } from '@/components/AgentConfig';
import { ConfirmButton } from '@/components/ConfirmButton';
import { LangfuseConfig } from '@/components/LangfuseConfig';
import { Tabs, TabsList, TabsTrigger, TabsContent } from '@/components/ui/tabs';
import { useProviders } from '@/hooks/settings/useProviders';
import { useAgentConfig } from '@/hooks/settings/useAgentConfig';
import { useSettingsStore } from '@/stores/settingsStore';
import { useServerHealth } from '@/hooks/useServerHealth';
import { AddProviderModal } from '@/components/AddProviderModal';

function ModelsTab() {
  const {
    providers,
    configuredProviders,
    loading,
    loadError,
    loadData,
    removeProvider,
  } = useProviders();

  const agentConfig = useAgentConfig();
  const loadProviderModels = useSettingsStore((s) => s.loadProviderModels);

  const [isAddOpen, setIsAddOpen] = useState(false);

  if (loading) {
    return <div className="rounded-lg border border-border bg-card p-6">Loading providers...</div>;
  }

  if (loadError) {
    return <InlineError message={loadError} onRetry={() => void loadData()} />;
  }

  if (providers.length === 0) {
    return (
      <EmptyState
        title="No providers found"
        message="Add a provider to start connecting your workspace tools."
        actionLabel="Reload providers"
        onAction={() => void loadData()}
        illustration={<div className="text-4xl">🔌</div>}
      />
    );
  }

  return (
    <div className="flex flex-col gap-6 flex-1 min-h-0">
      <AgentConfig {...agentConfig} />

      <div className="border-t border-border" />

      <div className="flex flex-col gap-3">
        <div className="flex items-center justify-between shrink-0">
          <h2 className="text-xl font-bold text-foreground">Providers</h2>
          <Button onClick={() => setIsAddOpen(true)}>Add Provider</Button>
        </div>

        <AddProviderModal
          open={isAddOpen}
          onOpenChange={setIsAddOpen}
          configuredProviderIds={configuredProviders.map((p) => p.id)}
          onDone={() => { void loadData(); void loadProviderModels(); }}
        />

        <div className="rounded-lg border border-border bg-card px-4 py-3">
          <div className="flex flex-wrap items-center gap-3">
            <span className="text-sm font-medium text-muted-foreground shrink-0">Connected:</span>
            {configuredProviders.length === 0 ? (
              <span className="text-sm text-muted-foreground">None yet.</span>
            ) : (
              configuredProviders.map((provider) => (
                <span key={provider.id} className="flex items-center gap-1 rounded-md border border-border px-2.5 py-1 text-sm">
                  {provider.name}
                  <ConfirmButton
                    title="Remove provider"
                    description={`Remove "${provider.name}" and its credentials?`}
                    confirmLabel="Remove"
                    onConfirm={() => removeProvider(provider.id)}
                    size="sm"
                    variant="ghost"
                  >
                    <HugeiconsIcon icon={Delete02Icon} size={13} className="text-destructive" />
                  </ConfirmButton>
                </span>
              ))
            )}
          </div>
        </div>
      </div>
    </div>
  );
}

function GeneralTab() {
  return (
    <div className="flex flex-col gap-6 flex-1 min-h-0">
      <LangfuseConfig />
    </div>
  );
}

export function SettingsPage() {
  const { status } = useServerHealth();
  const isConnected = status === 'connected';

  return (
    <div className="flex h-full flex-col overflow-hidden p-6">
      <Tabs defaultValue="models" className="flex-1 min-h-0 flex flex-col">
        <TabsList variant="line" className="shrink-0 w-full justify-start mb-4">
          <TabsTrigger value="models">Models</TabsTrigger>
          <TabsTrigger value="general">General</TabsTrigger>
        </TabsList>

        <TabsContent value="models" className="min-h-0 flex-1 overflow-y-auto overflow-x-hidden pb-6">
          {isConnected ? (
            <ModelsTab />
          ) : (
            <div className="rounded-lg border border-dashed border-border bg-card/50 px-4 py-6 text-center">
              <p className="text-sm text-muted-foreground">
                Connect to a server to manage providers and agents.
              </p>
            </div>
          )}
        </TabsContent>

        <TabsContent value="general" className="min-h-0 flex-1 overflow-y-auto overflow-x-hidden pb-6">
          <GeneralTab />
        </TabsContent>
      </Tabs>
    </div>
  );
}
