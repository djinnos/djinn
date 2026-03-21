import { useState } from 'react';
import { Button } from '@/components/ui/button';
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from '@/components/ui/collapsible';
import { ChevronDown, Key, Cpu, Sparkles, HelpCircle } from 'lucide-react';
import { cn } from '@/lib/utils';

interface ProviderOnboardingProps {
  oauthInProgress: boolean;
  onConnectChatGPT: () => void;
  onOpenCatalog: (filter?: string) => void;
}

export function ProviderOnboarding({ oauthInProgress, onConnectChatGPT, onOpenCatalog }: ProviderOnboardingProps) {
  const [faqOpen, setFaqOpen] = useState(false);

  return (
    <div className="flex flex-col items-center justify-start gap-6 py-8 px-4 max-w-lg mx-auto w-full">
      <div className="text-center space-y-1">
        <h2 className="text-xl font-semibold text-foreground">No providers connected</h2>
        <p className="text-sm text-muted-foreground">
          Djinn needs a model provider to run agents and tasks.
        </p>
      </div>

      {/* ChatGPT / Codex — featured */}
      <div className="w-full rounded-lg border border-primary/40 bg-card p-5 space-y-4">
        <div className="flex items-start justify-between gap-3">
          <div className="space-y-1">
            <div className="flex items-center gap-2">
              <Sparkles className="h-4 w-4 text-primary shrink-0" />
              <span className="font-semibold text-foreground">ChatGPT / Codex</span>
              <span className="rounded-full bg-primary/15 px-2 py-0.5 text-[10px] font-medium text-primary">
                No API key needed
              </span>
            </div>
            <p className="text-xs text-muted-foreground">
              Use your ChatGPT Plus, Pro, or Team plan
            </p>
          </div>
        </div>

        <ul className="space-y-1 text-xs text-muted-foreground">
          <li className="flex items-center gap-2 before:content-['•'] before:text-primary">
            Fast setup — sign in with your browser
          </li>
          <li className="flex items-center gap-2 before:content-['•'] before:text-primary">
            Best for coding agents (PRs, code review)
          </li>
          <li className="flex items-center gap-2 before:content-['•'] before:text-primary">
            Flat-rate with your existing ChatGPT plan
          </li>
        </ul>

        <Button
          className="w-full"
          onClick={onConnectChatGPT}
          disabled={oauthInProgress}
        >
          {oauthInProgress ? 'Waiting for browser...' : 'Continue with ChatGPT'}
        </Button>
      </div>

      {/* API Key */}
      <div className="w-full rounded-lg border border-border bg-card p-5 space-y-3">
        <div className="flex items-center gap-2">
          <Key className="h-4 w-4 text-muted-foreground shrink-0" />
          <span className="font-semibold text-foreground">Connect via API Key</span>
        </div>
        <p className="text-xs text-muted-foreground">
          Anthropic, Google, Azure, AWS, and more — pay per usage with your own key.
        </p>
        <Button variant="outline" className="w-full" onClick={() => onOpenCatalog()}>
          Use API Key
        </Button>
      </div>

      {/* Local Models */}
      <div className="w-full rounded-lg border border-border bg-card p-5 space-y-3">
        <div className="flex items-center gap-2">
          <Cpu className="h-4 w-4 text-muted-foreground shrink-0" />
          <span className="font-semibold text-foreground">Local Models</span>
        </div>
        <p className="text-xs text-muted-foreground">
          Run models on your machine with Ollama or LM Studio.
        </p>
        <Button variant="outline" className="w-full" onClick={() => onOpenCatalog('ollama')}>
          Connect Local
        </Button>
      </div>

      {/* FAQ */}
      <div className="w-full">
        <Collapsible open={faqOpen} onOpenChange={setFaqOpen}>
          <CollapsibleTrigger className="flex w-full items-center justify-between gap-2 rounded-md px-1 py-2 text-xs text-muted-foreground hover:text-foreground transition-colors">
            <span className="flex items-center gap-1.5">
              <HelpCircle className="h-3.5 w-3.5 shrink-0" />
              Why can&apos;t I use my Claude Pro or Gemini subscription?
            </span>
            <ChevronDown className={cn('h-3.5 w-3.5 shrink-0 transition-transform', faqOpen && 'rotate-180')} />
          </CollapsibleTrigger>
          <CollapsibleContent className="overflow-hidden">
            <div className="rounded-md border border-border bg-card/50 p-4 mt-1 text-xs text-muted-foreground space-y-2">
              <p>
                Both Anthropic and Google prohibit using their subscription OAuth tokens in
                third-party tools. Accounts have been suspended for this — it&apos;s not worth the
                risk.
              </p>
              <p>
                OpenAI is the only major provider that explicitly allows ChatGPT plan tokens in
                third-party applications, which is why Djinn supports it.
              </p>
              <p>
                To use Claude or Gemini models, add an API key instead — usage is billed at
                standard API rates.
              </p>
            </div>
          </CollapsibleContent>
        </Collapsible>
      </div>
    </div>
  );
}
