import { useState, useEffect } from 'react';
import {
  AlertDialog,
  AlertDialogContent,
  AlertDialogOverlay,
  AlertDialogPortal,
} from '@/components/ui/alert-dialog';
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

import { ArrowDown01Icon, CheckmarkCircle04Icon, AlertCircleIcon, HelpCircleIcon, Key01Icon, Loading02Icon, SparklesIcon, Cancel01Icon } from '@hugeicons/core-free-icons';
import { HugeiconsIcon } from '@hugeicons/react';
import { cn } from '@/lib/utils';
import {
  fetchProviderCatalog,
  startProviderOAuth,
  validateProviderApiKey,
  saveProviderCredentials,
  type Provider,
} from '@/api/server';

interface AddProviderModalProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  /** Provider IDs already configured — used to hide ChatGPT card if already connected */
  configuredProviderIds: string[];
  onDone: () => void;
}


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
    <div className="flex flex-col gap-4 rounded-xl border border-primary/30 bg-primary/5 p-5 h-full relative overflow-hidden">
      {/* subtle glow */}
      <div className="pointer-events-none absolute -right-8 -top-8 h-24 w-24 rounded-full bg-primary/10 blur-2xl" />

      <div className="flex items-center gap-2">
        <div className="flex h-7 w-7 shrink-0 items-center justify-center rounded-lg bg-primary/15">
          <HugeiconsIcon icon={SparklesIcon} size={14} className="text-primary" />
        </div>
        <span className="font-semibold text-sm text-foreground">ChatGPT / Codex</span>
        <span className="ml-auto rounded-full border border-primary/30 bg-primary/10 px-2 py-0.5 text-[10px] font-medium text-primary whitespace-nowrap">
          No API key
        </span>
      </div>

      <ul className="flex-1 space-y-1.5">
        {[
          'Sign in with browser — fast setup',
          'Best for coding agents & PRs',
          'Flat-rate with ChatGPT Plus/Pro plan',
        ].map((item) => (
          <li key={item} className="flex items-start gap-2 text-xs text-muted-foreground">
            <span className="mt-0.5 shrink-0 text-primary/60">›</span>
            {item}
          </li>
        ))}
      </ul>

      {error && (
        <p className="flex items-center gap-1.5 text-xs text-destructive">
          <HugeiconsIcon icon={AlertCircleIcon} size={14} className="shrink-0" />{error}
        </p>
      )}

      <Button className="w-full" onClick={() => void handleConnect()} disabled={pending}>
        {pending
          ? <><HugeiconsIcon icon={Loading02Icon} size={16} className="mr-2 animate-spin" />Waiting for browser...</>
          : 'Continue with ChatGPT'}
      </Button>
    </div>
  );
}

/** Pure form fields — no card wrapper. Parent decides surrounding layout. */
function ApiKeyFields({ onDone }: { onDone: () => void }) {
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
      .catch(() => {});
  }, []);

  const selectedProvider = providers.find((p) => p.id === selectedId);

  const handleProviderChange = (v: string) => {
    setSelectedId(v);
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
      if (result.valid) { setValidated(true); }
      else { setError(result.error ?? 'Invalid API key'); }
    } finally { setValidating(false); }
  };

  const handleSave = async () => {
    if (!selectedId || !apiKey.trim()) return;
    if (!validated) {
      await handleValidate();
      if (error || !validated) return;
    }
    setSaving(true);
    try {
      await saveProviderCredentials(selectedId, apiKey.trim());
      onDone();
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Failed to save credentials');
    } finally { setSaving(false); }
  };

  const busy = validating || saving;

  return (
    <div className="flex flex-col gap-3">
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
          <Input
            type="password"
            placeholder={`${selectedProvider.name} API key`}
            value={apiKey}
            className="text-sm"
            onChange={(e) => { setApiKey(e.target.value); setValidated(false); setError(null); }}
            onBlur={() => void handleValidate()}
          />
          {validated && (
            <p className="flex items-center gap-1.5 text-xs text-emerald-500">
              <HugeiconsIcon icon={CheckmarkCircle04Icon} size={14} />API key is valid
            </p>
          )}
          {error && (
            <p className="flex items-center gap-1.5 text-xs text-destructive">
              <HugeiconsIcon icon={AlertCircleIcon} size={14} className="shrink-0" />{error}
            </p>
          )}
        </>
      )}

      <Button
        className="w-full"
        disabled={!selectedId || !apiKey.trim() || busy}
        onClick={() => void handleSave()}
      >
        {saving
          ? <><HugeiconsIcon icon={Loading02Icon} size={16} className="mr-2 animate-spin" />Saving...</>
          : 'Use API Key'}
      </Button>
    </div>
  );
}

function FaqSection() {
  const [open, setOpen] = useState(false);
  return (
    <Collapsible open={open} onOpenChange={setOpen}>
      <CollapsibleTrigger className="flex w-full items-center gap-2 py-2 text-xs text-muted-foreground transition-colors hover:text-foreground">
        <HugeiconsIcon icon={HelpCircleIcon} size={14} className="shrink-0" />
        <span>Why can&apos;t I use my Claude Pro or Gemini subscription?</span>
        <HugeiconsIcon icon={ArrowDown01Icon} size={14} className={cn('ml-auto shrink-0 transition-transform duration-200', open && 'rotate-180')} />
      </CollapsibleTrigger>
      <CollapsibleContent className="overflow-hidden">
        <div className="space-y-1.5 pb-1 pl-5 text-xs text-muted-foreground">
          <p>Both Anthropic and Google prohibit using their subscription OAuth tokens in third-party tools. Accounts have been suspended for this.</p>
          <p>OpenAI explicitly allows ChatGPT plan tokens in third-party apps. For Claude or Gemini models, use an API key instead.</p>
        </div>
      </CollapsibleContent>
    </Collapsible>
  );
}

export function AddProviderModal({ open, onOpenChange, configuredProviderIds, onDone }: AddProviderModalProps) {
  const chatGPTConnected = configuredProviderIds.includes('openai');

  const handleDone = () => {
    onDone();
    onOpenChange(false);
  };

  return (
    <AlertDialog open={open} onOpenChange={onOpenChange}>
      <AlertDialogPortal>
        <AlertDialogOverlay />
        <AlertDialogContent className={cn('p-0 overflow-hidden', chatGPTConnected ? 'max-w-sm w-full' : 'max-w-2xl w-full')}>

          {/* Header */}
          <div className="flex items-center justify-between border-b border-border px-5 py-4">
            <h2 className="text-sm font-semibold tracking-tight">Add Provider</h2>
            <Button variant="ghost" size="icon" className="h-7 w-7 -mr-1" onClick={() => onOpenChange(false)}>
              <HugeiconsIcon icon={Cancel01Icon} size={14} />
            </Button>
          </div>

          {chatGPTConnected ? (
            /* ── Single-column: no inner card, form sits directly in modal body ── */
            <div className="flex flex-col gap-5 px-5 py-5">

              {/* Section header + provider badges */}
              <div className="flex items-center gap-2">
                <div className="flex h-6 w-6 shrink-0 items-center justify-center rounded-md bg-muted">
                  <HugeiconsIcon icon={Key01Icon} size={12} className="text-muted-foreground" />
                </div>
                <div>
                  <span className="text-sm font-medium text-foreground">Connect via API Key</span>
                  <p className="text-xs text-muted-foreground">Anthropic, Google, Azure, AWS, and more.</p>
                </div>
              </div>

              <ApiKeyFields onDone={handleDone} />

              <div className="border-t border-border pt-1">
                <FaqSection />
              </div>
            </div>

          ) : (
            /* ── Two-column: ChatGPT card | divider | API key section ── */
            <div className="flex flex-col gap-4 p-5">
              <div className="grid grid-cols-[1fr_auto_1fr] items-stretch gap-0">

                <ChatGPTCard onDone={handleDone} />

                {/* Vertical "or" divider */}
                <div className="flex flex-col items-center justify-center px-4">
                  <div className="w-px flex-1 bg-border" />
                  <span className="py-2 text-[10px] font-medium uppercase tracking-widest text-muted-foreground">or</span>
                  <div className="w-px flex-1 bg-border" />
                </div>

                {/* API Key column — no card wrapper */}
                <div className="flex flex-col gap-3">
                  <div className="flex items-center gap-2">
                    <div className="flex h-7 w-7 shrink-0 items-center justify-center rounded-lg bg-muted">
                      <HugeiconsIcon icon={Key01Icon} size={14} className="text-muted-foreground" />
                    </div>
                    <span className="text-sm font-semibold text-foreground">API Key</span>
                  </div>

                  <p className="text-xs text-muted-foreground">Anthropic, Google, Azure, AWS, and more.</p>

                  <ApiKeyFields onDone={handleDone} />
                </div>
              </div>

              <div className="border-t border-border pt-1">
                <FaqSection />
              </div>
            </div>
          )}

        </AlertDialogContent>
      </AlertDialogPortal>
    </AlertDialog>
  );
}
