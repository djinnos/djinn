import { InformationCircleIcon } from '@hugeicons/core-free-icons';
import { HugeiconsIcon } from '@hugeicons/react';

import logoSvg from '@/assets/logo.svg';
import { CodexSignInCard } from '@/components/CodexSignInCard';
import { useProviderGateStore } from '@/stores/providerGateStore';

/**
 * Onboarding gate rendered when no provider credential has been configured.
 * Post-migration the UI only offers self-serve Codex OAuth; every other
 * provider (Anthropic, OpenAI API, Google, Azure, AWS Bedrock, Vertex AI)
 * is operator-provisioned via Helm values (`secrets.providers.*`).
 */
export function ProviderOnboarding() {
  const { refresh } = useProviderGateStore();

  return (
    <main className="flex min-h-screen flex-col items-center justify-center bg-background text-foreground px-6 py-12">
      <div className="flex w-full max-w-xl flex-col items-center gap-10">
        <div className="relative">
          <div
            className="pointer-events-none absolute left-1/2 top-1/2 -translate-x-1/2 -translate-y-1/2 h-20 w-20 rounded-full bg-purple-400/40"
            style={{ filter: 'blur(50px)' }}
          />
          <img
            src={logoSvg}
            alt="Djinn"
            className="relative h-20 w-auto drop-shadow-[0_0_40px_rgba(168,139,250,0.35)]"
          />
        </div>

        <div className="text-center space-y-2">
          <h2 className="text-2xl font-semibold">Connect a model provider</h2>
          <p className="text-base text-muted-foreground">
            Djinn needs a model provider to run agents and tasks.
          </p>
        </div>

        <CodexSignInCard className="w-full" onConnected={() => void refresh()} />

        <div className="w-full rounded-lg border border-border bg-card/50 p-5 text-sm text-muted-foreground">
          <div className="flex items-start gap-3">
            <HugeiconsIcon
              icon={InformationCircleIcon}
              size={18}
              className="shrink-0 mt-0.5 text-muted-foreground"
            />
            <div className="space-y-2">
              <p className="text-foreground font-medium">
                Need Anthropic, OpenAI, or another API-key provider?
              </p>
              <p>
                API-key providers are provisioned per-deployment. Your operator sets them via
                Helm values (
                <code className="rounded bg-muted px-1 py-0.5 text-xs">secrets.providers.*</code>
                ) and they&apos;re bootstrapped into the encrypted vault at server start.
              </p>
            </div>
          </div>
        </div>
      </div>
    </main>
  );
}
