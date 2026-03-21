import { useState } from 'react';
import { Button } from '@/components/ui/button';
import { InlineError } from '@/components/InlineError';
import { EmptyState } from '@/components/EmptyState';
import { AgentConfig } from '@/components/AgentConfig';
import { ConfirmButton } from '@/components/ConfirmButton';
import { useProviders } from '@/hooks/settings/useProviders';
import { useAgentConfig } from '@/hooks/settings/useAgentConfig';
import { AddProviderModal } from '@/components/AddProviderModal';

function ProvidersSettings() {
  const {
    providers,
    configuredProviders,
    loading,
    loadError,
    loadData,
    removeProvider,
  } = useProviders();

  const agentConfig = useAgentConfig();

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

      <div className="flex flex-col gap-4">
        <div className="flex items-center justify-between shrink-0">
          <h2 className="text-lg font-semibold text-foreground">Providers</h2>
          <Button onClick={() => setIsAddOpen(true)}>Add Provider</Button>
        </div>

        <AddProviderModal
          open={isAddOpen}
          onOpenChange={setIsAddOpen}
          configuredProviderIds={configuredProviders.map((p) => p.id)}
          onDone={() => void loadData()}
        />

        <div className="space-y-2 shrink-0">
          {configuredProviders.map((provider) => (
            <div key={provider.id} className="flex items-center justify-between rounded-lg border border-border bg-card p-4">
              <div>
                <p className="font-medium">{provider.name}</p>
                <p className="text-xs text-muted-foreground">Configured</p>
              </div>
              <ConfirmButton
                title="Remove provider"
                description={`Remove "${provider.name}" and its credentials?`}
                confirmLabel="Remove"
                onConfirm={() => removeProvider(provider.id)}
                size="sm"
              >
                Remove
              </ConfirmButton>
            </div>
          ))}
          {configuredProviders.length === 0 && <p className="text-sm text-muted-foreground">No providers configured yet.</p>}
        </div>
      </div>
    </div>
  );
}

export function SettingsPage() {
  return (
    <div className="flex h-full flex-col overflow-hidden p-6">
      <section className="min-h-0 min-w-0 flex-1 flex flex-col overflow-x-hidden overflow-y-auto pb-6">
        <ProvidersSettings />
      </section>
    </div>
  );
}
