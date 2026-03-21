import { useEffect, useState } from 'react';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from '@/components/ui/collapsible';
import {
  Combobox,
  ComboboxCollection,
  ComboboxContent,
  ComboboxEmpty,
  ComboboxInput,
  ComboboxItem,
  ComboboxList,
} from '@/components/ui/combobox';

import { ArrowDown01Icon, CheckmarkCircle04Icon, AlertCircleIcon, HelpCircleIcon, Key01Icon, Loading02Icon, SparklesIcon } from '@hugeicons/core-free-icons';
import { HugeiconsIcon } from '@hugeicons/react';
import { cn } from '@/lib/utils';
import logoSvg from '@/assets/logo.svg';
import {
  fetchProviderCatalog,
  startProviderOAuth,
  validateProviderApiKey,
  saveProviderCredentials,
  type Provider,
} from '@/api/server';
import { useProviderGateStore } from '@/stores/providerGateStore';

function ChatGPTCard({ onDone }: { onDone: () => void }) {
  const [pending, setPending] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const handleConnect = async () => {
    setPending(true);
    setError(null);
    try {
      const result = await startProviderOAuth('openai');
      if (result.success) {
        onDone();
      } else {
        setError(result.error ?? 'OAuth flow failed');
      }
    } catch (e) {
      setError(e instanceof Error ? e.message : 'OAuth flow failed');
    } finally {
      setPending(false);
    }
  };

  return (
    <div className="flex flex-col gap-4 rounded-xl border border-primary/40 bg-card p-6 h-full">
      <div className="flex items-center gap-2 flex-wrap">
        <HugeiconsIcon icon={SparklesIcon} size={16} className="text-primary shrink-0" />
        <span className="font-semibold text-foreground">ChatGPT / Codex</span>
        <span className="rounded-full bg-green-500/15 px-2 py-0.5 text-[10px] font-medium text-green-500 whitespace-nowrap">
          No API key needed
        </span>
      </div>

      <p className="text-xs text-muted-foreground">
        Use your ChatGPT Plus, Pro, or Team plan — sign in with your browser.
      </p>

      <ul className="space-y-1.5 text-xs text-muted-foreground flex-1">
        {[
          'Fast setup — sign in with your browser',
          'Best for coding agents (PRs, code review)',
          'Flat-rate with your existing ChatGPT plan',
        ].map((item) => (
          <li key={item} className="flex items-start gap-2">
            <span className="mt-0.5 text-primary">•</span>
            {item}
          </li>
        ))}
      </ul>

      {error && (
        <p className="flex items-center gap-1.5 text-xs text-destructive">
          <HugeiconsIcon icon={AlertCircleIcon} size={14} className="shrink-0" />
          {error}
        </p>
      )}

      <Button className="w-full" onClick={() => void handleConnect()} disabled={pending}>
        {pending ? (
          <><HugeiconsIcon icon={Loading02Icon} size={16} className="mr-2 animate-spin" />Waiting for browser...</>
        ) : (
          'Continue with ChatGPT'
        )}
      </Button>
    </div>
  );
}

function ApiKeyCard({ onDone }: { onDone: () => void }) {
  const [providers, setProviders] = useState<Provider[]>([]);
  const [selectedId, setSelectedId] = useState('');
  const [apiKey, setApiKey] = useState('');
  const [validating, setValidating] = useState(false);
  const [saving, setSaving] = useState(false);
  const [validated, setValidated] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    fetchProviderCatalog()
      .then((catalog) => setProviders(catalog.filter((p) => p.requires_api_key)))
      .catch(() => {/* silently fail — user can retry via provider change */});
  }, []);

  const selectedProvider = providers.find((p) => p.id === selectedId);

  const handleProviderChange = (value: string) => {
    setSelectedId(value);
    setApiKey('');
    setValidated(false);
    setError(null);
  };

  const handleValidate = async () => {
    if (!selectedId || !apiKey.trim()) return;
    setValidating(true);
    setError(null);
    try {
      const result = await validateProviderApiKey(selectedId, apiKey.trim());
      if (result.valid) {
        setValidated(true);
      } else {
        setError(result.error ?? 'Invalid API key');
      }
    } finally {
      setValidating(false);
    }
  };

  const handleSave = async () => {
    if (!selectedId || !apiKey.trim()) return;
    if (!validated) {
      await handleValidate();
      if (error) return;
    }
    setSaving(true);
    try {
      await saveProviderCredentials(selectedId, apiKey.trim());
      onDone();
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Failed to save credentials');
    } finally {
      setSaving(false);
    }
  };

  const busy = validating || saving;

  return (
    <div className="flex flex-col gap-4 rounded-xl border border-border bg-card p-6 h-full">
      <div className="flex items-center gap-2">
        <HugeiconsIcon icon={Key01Icon} size={16} className="text-muted-foreground shrink-0" />
        <span className="font-semibold text-foreground">API Key</span>
      </div>

      <p className="text-xs text-muted-foreground">
        Anthropic, Google, Azure, AWS, and more — pay per usage.
      </p>

      <div className="flex flex-col gap-3 flex-1">
        <Combobox
          items={providers}
          value={selectedId}
          onValueChange={(v) => v && handleProviderChange(v)}
        >
          <ComboboxInput placeholder="Search provider..." className="w-full" showClear={false} />
          <ComboboxContent>
            <ComboboxEmpty>No providers found.</ComboboxEmpty>
            <ComboboxList>
              <ComboboxCollection>
                {(p: Provider) => (
                  <ComboboxItem key={p.id} value={p.id}>{p.name}</ComboboxItem>
                )}
              </ComboboxCollection>
            </ComboboxList>
          </ComboboxContent>
        </Combobox>

        {selectedProvider && (
          <>
            <div className="flex gap-2">
              <Input
                type="password"
                placeholder={`${selectedProvider.name} API key`}
                value={apiKey}
                className="flex-1 text-sm"
                onChange={(e) => {
                  setApiKey(e.target.value);
                  setValidated(false);
                  setError(null);
                }}
                onBlur={() => void handleValidate()}
              />
            </div>

            {validated && (
              <p className="flex items-center gap-1.5 text-xs text-emerald-500">
                <HugeiconsIcon icon={CheckmarkCircle04Icon} size={14} />
                API key is valid
              </p>
            )}
            {error && (
              <p className="flex items-center gap-1.5 text-xs text-destructive">
                <HugeiconsIcon icon={AlertCircleIcon} size={14} className="shrink-0" />
                {error}
              </p>
            )}
          </>
        )}
      </div>

      <Button
        variant="outline"
        className="w-full"
        disabled={!selectedId || !apiKey.trim() || busy}
        onClick={() => void handleSave()}
      >
        {saving ? (
          <><HugeiconsIcon icon={Loading02Icon} size={16} className="mr-2 animate-spin" />Saving...</>
        ) : (
          'Use API Key'
        )}
      </Button>
    </div>
  );
}

export function ProviderOnboarding() {
  const { refresh } = useProviderGateStore();
  const [faqOpen, setFaqOpen] = useState(false);

  const handleDone = () => void refresh();

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
          <h2 className="text-xl font-semibold">Connect a model provider</h2>
          <p className="text-sm text-muted-foreground">
            Djinn needs a model provider to run agents and tasks.
          </p>
        </div>

        {/* Two-column cards */}
        <div className="grid w-full grid-cols-2 gap-4">
          <ChatGPTCard onDone={handleDone} />
          <ApiKeyCard onDone={handleDone} />
        </div>

        {/* FAQ */}
        <div className="w-full">
          <Collapsible open={faqOpen} onOpenChange={setFaqOpen}>
            <CollapsibleTrigger className="flex w-full items-center justify-between gap-2 rounded-md px-1 py-2 text-xs text-muted-foreground hover:text-foreground transition-colors">
              <span className="flex items-center gap-1.5">
                <HugeiconsIcon icon={HelpCircleIcon} size={14} className="shrink-0" />
                Why can&apos;t I use my Claude Pro or Gemini subscription?
              </span>
              <HugeiconsIcon icon={ArrowDown01Icon} size={14} className={cn('shrink-0 transition-transform', faqOpen && 'rotate-180')} />
            </CollapsibleTrigger>
            <CollapsibleContent className="overflow-hidden">
              <div className="rounded-md border border-border bg-card/50 p-4 mt-1 text-xs text-muted-foreground space-y-2">
                <p>
                  Both Anthropic and Google prohibit using their subscription OAuth tokens in
                  third-party tools. Accounts have been suspended for this.
                </p>
                <p>
                  OpenAI explicitly allows ChatGPT plan tokens in third-party apps, which is why
                  Djinn supports it. For Claude or Gemini models, use an API key instead.
                </p>
              </div>
            </CollapsibleContent>
          </Collapsible>
        </div>

      </div>
    </main>
  );
}
