import { Delete02Icon, LockIcon } from '@hugeicons/core-free-icons';
import { HugeiconsIcon } from '@hugeicons/react';
import { InlineError } from '@/components/InlineError';
import { CodexSignInCard } from '@/components/CodexSignInCard';
import { ConfirmButton } from '@/components/ConfirmButton';
import { Tabs, TabsContent, TabsList, TabsTrigger } from '@/components/ui/tabs';
import { ModelSection } from '@/components/userConfig/ModelSection';
import { AiPolicyTab } from '@/components/settings/AiPolicyTab';
import { useAuthUser } from '@/components/AuthGate';
import { SELF_TARGET } from '@/api/userConfig';
import { useProviders } from '@/hooks/settings/useProviders';
import { useUserSettings } from '@/hooks/settings/useUserSettings';
import { useServerHealth } from '@/hooks/useServerHealth';
import { cn } from '@/lib/utils';

/** Connections tab — provider auth only (connect / disconnect). No model picking. */
function ConnectionsTab() {
  const {
    configuredProviders,
    loading,
    loadError,
    loadData,
    removeProvider,
    isSelfServeProvider,
  } = useProviders();

  if (loading) {
    return <div className="rounded-lg border border-border bg-card p-6">Loading providers...</div>;
  }

  if (loadError) {
    return <InlineError message={loadError} onRetry={() => void loadData()} />;
  }

  // Codex OAuth is folded into the `openai` provider via builtin merge
  // (chatgpt_codex.merge_into = "openai"), and openai itself has no native
  // OAuth — so `oauth` in openai's connection_methods means Codex is signed in.
  const openaiProvider = configuredProviders.find((p) => p.id === 'openai');
  const codexConnected = openaiProvider?.connection_methods.includes('oauth') ?? false;
  // When the codex credential was revoked (a 401 during a run) the server reports
  // openai disconnected + carries the reason; surface it so the user reconnects.
  const codexRevokedReason = openaiProvider?.revoked_reason;

  return (
    <div className="flex flex-col gap-3">
      <div>
        <h2 className="text-xl font-bold text-foreground">ChatGPT / Codex</h2>
        <p className="text-sm text-muted-foreground">
          Sign in with your ChatGPT subscription. All other providers (Anthropic, OpenAI API,
          Google, Azure, AWS, Vertex AI) are provisioned by your operator via Helm values —
          they show up automatically once configured.
        </p>
      </div>

      {/* Always render: shows the sign-in CTA when disconnected, and the
          connected state (with a Remove/Disconnect button) when connected. */}
      <CodexSignInCard
        alreadyConnected={codexConnected}
        revokedReason={codexRevokedReason}
        onConnected={() => void loadData()}
      />

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
  );
}

function EmptyConnectedMessage() {
  return (
    <span className="text-sm text-muted-foreground">
      None yet. Sign in above, or ask your operator to set a Helm-managed API key.
    </span>
  );
}

/** Model Roles tab — per-user, per-role model lanes for the signed-in caller. */
function ModelRolesTab() {
  return (
    <div className="flex flex-col gap-6">
      {/* Per-user, per-ROLE model lanes for the signed-in caller (self mode). */}
      <ModelSection targetId={SELF_TARGET} />
    </div>
  );
}

/** Preferences tab — account-level toggles (auto-approve PRs). */
function PreferencesTab() {
  const { settings, loading, saving, error, setAutoApprovePrs } = useUserSettings();

  const showSignIn = !loading && !settings && error?.toLowerCase().includes('sign in');

  return (
    <div className="rounded-lg border border-border bg-card p-6">
      <h2 className="mb-1 text-base font-semibold">Account preferences</h2>
      <p className="mb-4 text-xs text-muted-foreground">
        Per-user toggles backed by your GitHub identity.
      </p>

      {loading ? (
        <span className="text-sm text-muted-foreground">Loading…</span>
      ) : showSignIn ? (
        <span className="text-sm text-muted-foreground">
          Sign in with GitHub to manage account preferences.
        </span>
      ) : error ? (
        <InlineError message={error} />
      ) : settings ? (
        <div className="flex items-start justify-between gap-4">
          <div className="min-w-0">
            <label htmlFor="auto-approve-prs" className="text-sm font-medium">
              Auto-approve PRs when ready to merge
            </label>
            <p className="mt-1 text-xs text-muted-foreground">
              When a PR you (or a task you created) opened has CI green, no
              conflicts, and is otherwise mergeable, Djinn will post an APPROVE
              review using your GitHub token so branch protection's approval
              requirement is satisfied automatically. Background-agent-created
              tasks (no creator on file) are never auto-approved. Approvals are
              pinned to the head commit, so a fresh push invalidates them.
            </p>
          </div>
          <button
            id="auto-approve-prs"
            type="button"
            role="switch"
            aria-checked={settings.autoApprovePrs}
            disabled={saving}
            onClick={() => void setAutoApprovePrs(!settings.autoApprovePrs)}
            className={cn(
              'relative inline-flex h-6 w-11 shrink-0 cursor-pointer items-center rounded-full border transition-colors',
              settings.autoApprovePrs
                ? 'border-primary bg-primary'
                : 'border-border bg-muted',
              saving && 'opacity-60',
            )}
          >
            <span
              className={cn(
                'inline-block h-4 w-4 transform rounded-full bg-white shadow transition-transform',
                settings.autoApprovePrs ? 'translate-x-6' : 'translate-x-1',
              )}
            />
          </button>
        </div>
      ) : null}
    </div>
  );
}

export function SettingsPage() {
  const { status } = useServerHealth();
  const isConnected = status === 'connected';
  const isAdmin = useAuthUser()?.isAdmin ?? false;

  return (
    <div className="flex h-full flex-col overflow-hidden p-6">
      {isConnected ? (
        <Tabs defaultValue="connections" className="min-h-0 flex-1">
          <TabsList>
            <TabsTrigger value="connections">Connections</TabsTrigger>
            <TabsTrigger value="model-roles">Model Roles</TabsTrigger>
            <TabsTrigger value="preferences">Preferences</TabsTrigger>
            {isAdmin && <TabsTrigger value="ai-policy">AI Policy</TabsTrigger>}
          </TabsList>
          <div className="min-h-0 flex-1 overflow-y-auto overflow-x-hidden pt-4 pb-6">
            <TabsContent value="connections">
              <ConnectionsTab />
            </TabsContent>
            <TabsContent value="model-roles">
              <ModelRolesTab />
            </TabsContent>
            <TabsContent value="preferences">
              <PreferencesTab />
            </TabsContent>
            {isAdmin && (
              <TabsContent value="ai-policy">
                <AiPolicyTab />
              </TabsContent>
            )}
          </div>
        </Tabs>
      ) : (
        <div className="rounded-lg border border-dashed border-border bg-card/50 px-4 py-6 text-center">
          <p className="text-sm text-muted-foreground">
            Connect to a server to manage providers and agents.
          </p>
        </div>
      )}
    </div>
  );
}
