import { Delete02Icon, LockIcon } from '@hugeicons/core-free-icons';
import { HugeiconsIcon } from '@hugeicons/react';
import { InlineError } from '@/components/InlineError';
import { AgentConfig } from '@/components/AgentConfig';
import { CodexSignInCard } from '@/components/CodexSignInCard';
import { ConfirmButton } from '@/components/ConfirmButton';
import { LangfuseConfig } from '@/components/LangfuseConfig';
import { Tabs, TabsList, TabsTrigger, TabsContent } from '@/components/ui/tabs';
import { useProviders } from '@/hooks/settings/useProviders';
import { useAgentConfig } from '@/hooks/settings/useAgentConfig';
import { useServerHealth } from '@/hooks/useServerHealth';

function ModelsTab() {
  const {
    configuredProviders,
    loading,
    loadError,
    loadData,
    removeProvider,
    isSelfServeProvider,
  } = useProviders();

  const agentConfig = useAgentConfig();

  if (loading) {
    return <div className="rounded-lg border border-border bg-card p-6">Loading providers...</div>;
  }

  if (loadError) {
    return <InlineError message={loadError} onRetry={() => void loadData()} />;
  }

  // Codex OAuth is folded into the `openai` provider via builtin merge
  // (chatgpt_codex.merge_into = "openai"), and openai itself has no native
  // OAuth — so `oauth` in openai's connection_methods means Codex is signed in.
  const codexConnected = configuredProviders.some(
    (p) => p.id === 'openai' && p.connection_methods.includes('oauth'),
  );

  return (
    <div className="flex flex-col gap-6 flex-1 min-h-0">
      <AgentConfig {...agentConfig} />

      <div className="border-t border-border" />

      <div className="flex flex-col gap-3">
        <div>
          <h2 className="text-xl font-bold text-foreground">ChatGPT / Codex</h2>
          <p className="text-sm text-muted-foreground">
            Sign in with your ChatGPT subscription. All other providers (Anthropic, OpenAI API,
            Google, Azure, AWS, Vertex AI) are provisioned by your operator via Helm values —
            they show up automatically once configured.
          </p>
        </div>

        {!codexConnected && <CodexSignInCard onConnected={() => void loadData()} />}

        <div className="rounded-lg border border-border bg-card px-4 py-3">
          <div className="flex flex-wrap items-center gap-3">
            <span className="text-sm font-medium text-muted-foreground shrink-0">Connected:</span>
            {configuredProviders.length === 0 ? (
              <EmptyConnectedMessage />
            ) : (
              configuredProviders.map((provider) => {
                const removable = isSelfServeProvider(provider.id);
                return (
                  <span
                    key={provider.id}
                    className="flex items-center gap-1 rounded-md border border-border px-2.5 py-1 text-sm"
                    title={
                      removable
                        ? undefined
                        : 'Provisioned via deployment (Helm). Ask your operator to unset the key.'
                    }
                  >
                    {provider.name}
                    {removable ? (
                      <ConfirmButton
                        title="Remove provider"
                        description={`Sign out of "${provider.name}" and delete the stored tokens?`}
                        confirmLabel="Remove"
                        onConfirm={() => removeProvider(provider.id)}
                        size="sm"
                        variant="ghost"
                      >
                        <HugeiconsIcon
                          icon={Delete02Icon}
                          size={13}
                          className="text-destructive"
                        />
                      </ConfirmButton>
                    ) : (
                      <HugeiconsIcon
                        icon={LockIcon}
                        size={13}
                        className="text-muted-foreground"
                        aria-label="Provisioned via deployment"
                      />
                    )}
                  </span>
                );
              })
            )}
          </div>
        </div>
      </div>
    </div>
  );
}

function EmptyConnectedMessage() {
  return (
    <span className="text-sm text-muted-foreground">
      None yet. Sign in above, or ask your operator to set a Helm-managed API key.
    </span>
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
