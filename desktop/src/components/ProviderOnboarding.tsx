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

/** Well-known providers shown as chips in the API key section. */
const PROVIDER_CHIPS = [
  { id: 'anthropic', label: 'Anthropic', color: 'bg-orange-500/15 text-orange-400' },
  { id: 'google', label: 'Google AI', color: 'bg-blue-500/15 text-blue-400' },
  { id: 'azure_openai', label: 'Azure', color: 'bg-sky-500/15 text-sky-400' },
  { id: 'aws_bedrock', label: 'AWS Bedrock', color: 'bg-amber-500/15 text-amber-400' },
  { id: 'gcp_vertex_ai', label: 'Vertex AI', color: 'bg-emerald-500/15 text-emerald-400' },
];

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
    <div className="relative flex flex-col gap-4 rounded-2xl border border-primary/40 bg-gradient-to-br from-primary/[0.06] to-transparent p-7 h-full overflow-hidden">
      {/* glow */}
      <div className="pointer-events-none absolute -right-8 -top-8 h-28 w-28 rounded-full bg-primary/20 blur-3xl" />
      <div className="pointer-events-none absolute -left-6 -bottom-6 h-20 w-20 rounded-full bg-primary/10 blur-3xl" />

      <div className="flex items-center gap-3">
        <div className="flex h-10 w-10 shrink-0 items-center justify-center rounded-xl bg-primary/15">
          <HugeiconsIcon icon={SparklesIcon} size={20} className="text-primary" />
        </div>
        <div>
          <h3 className="text-base font-semibold text-foreground">ChatGPT / Codex</h3>
          <p className="text-xs text-muted-foreground">Browser sign-in</p>
        </div>
      </div>

      <p className="text-sm leading-relaxed text-muted-foreground flex-1">
        Sign in with your browser. Works with ChatGPT Plus, Pro, or Team — no API key needed.
      </p>

      <span className="inline-flex self-start rounded-full bg-green-500/15 px-3 py-1 text-xs font-medium text-green-400">
        Flat-rate pricing
      </span>

      {error && (
        <p className="flex items-center gap-2 text-sm text-destructive">
          <HugeiconsIcon icon={AlertCircleIcon} size={16} className="shrink-0" />
          {error}
        </p>
      )}

      <Button size="lg" className="w-full text-sm" onClick={() => void handleConnect()} disabled={pending}>
        {pending ? (
          <><HugeiconsIcon icon={Loading02Icon} size={18} className="mr-2 animate-spin" />Waiting for browser...</>
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
    <div className="flex flex-col gap-4 rounded-2xl border border-border bg-card p-7 h-full">
      <div className="flex items-center gap-3">
        <div className="flex h-10 w-10 shrink-0 items-center justify-center rounded-xl bg-muted">
          <HugeiconsIcon icon={Key01Icon} size={20} className="text-muted-foreground" />
        </div>
        <div>
          <h3 className="text-base font-semibold text-foreground">API Key</h3>
          <p className="text-xs text-muted-foreground">Pay per token</p>
        </div>
      </div>

      {/* Provider chips */}
      <div className="flex flex-wrap gap-2">
        {PROVIDER_CHIPS.map((chip) => (
          <button
            key={chip.id}
            type="button"
            className={cn(
              'rounded-full px-3 py-1 text-xs font-medium transition-opacity hover:opacity-80',
              chip.color,
            )}
            onClick={() => handleProviderChange(chip.id)}
          >
            {chip.label}
          </button>
        ))}
      </div>

      <div className="flex flex-col gap-4 flex-1">
        <Combobox
          items={providers}
          value={selectedId}
          onValueChange={(v) => v && handleProviderChange(v)}
        >
          <ComboboxInput placeholder="Select a provider..." className="w-full h-10" showClear={false} />
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
            <Input
              type="password"
              placeholder={`${selectedProvider.name} API key`}
              value={apiKey}
              className="h-10"
              onChange={(e) => {
                setApiKey(e.target.value);
                setValidated(false);
                setError(null);
              }}
              onBlur={() => void handleValidate()}
            />

            {validated && (
              <p className="flex items-center gap-2 text-sm text-emerald-500">
                <HugeiconsIcon icon={CheckmarkCircle04Icon} size={16} />
                API key is valid
              </p>
            )}
            {error && (
              <p className="flex items-center gap-2 text-sm text-destructive">
                <HugeiconsIcon icon={AlertCircleIcon} size={16} className="shrink-0" />
                {error}
              </p>
            )}
          </>
        )}
      </div>

      <Button
        variant="outline"
        size="lg"
        className="w-full text-sm"
        disabled={!selectedId || !apiKey.trim() || busy}
        onClick={() => void handleSave()}
      >
        {saving ? (
          <><HugeiconsIcon icon={Loading02Icon} size={18} className="mr-2 animate-spin" />Saving...</>
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
      <div className="flex w-full max-w-3xl flex-col items-center gap-10">

        {/* Logo */}
        <div className="relative">
          <div
            className="pointer-events-none absolute left-1/2 top-1/2 -translate-x-1/2 -translate-y-1/2 h-20 w-20 rounded-full bg-purple-400/40"
            style={{ filter: 'blur(50px)' }}
          />
          <img src={logoSvg} alt="Djinn" className="relative h-20 w-auto drop-shadow-[0_0_40px_rgba(168,139,250,0.35)]" />
        </div>

        {/* Header */}
        <div className="text-center space-y-2">
          <h2 className="text-2xl font-semibold">Connect a model provider</h2>
          <p className="text-base text-muted-foreground">
            Djinn needs a model provider to run agents and tasks.
          </p>
        </div>

        {/* Two-column cards */}
        <div className="grid w-full grid-cols-2 gap-5">
          <ChatGPTCard onDone={handleDone} />
          <ApiKeyCard onDone={handleDone} />
        </div>

        {/* FAQ */}
        <div className="w-full">
          <Collapsible open={faqOpen} onOpenChange={setFaqOpen}>
            <CollapsibleTrigger className="flex w-full items-center justify-between gap-2 rounded-md px-1 py-2.5 text-sm text-muted-foreground hover:text-foreground transition-colors">
              <span className="flex items-center gap-2">
                <HugeiconsIcon icon={HelpCircleIcon} size={16} className="shrink-0" />
                Why can&apos;t I use my Claude Pro or Gemini subscription?
              </span>
              <HugeiconsIcon icon={ArrowDown01Icon} size={16} className={cn('shrink-0 transition-transform', faqOpen && 'rotate-180')} />
            </CollapsibleTrigger>
            <CollapsibleContent className="overflow-hidden">
              <div className="rounded-lg border border-border bg-card/50 p-5 mt-1 text-sm text-muted-foreground space-y-2">
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
