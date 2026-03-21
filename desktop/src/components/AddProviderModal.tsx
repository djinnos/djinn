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
import {
  Sparkles, Key, ChevronDown, HelpCircle,
  Loader2, CheckCircle2, AlertCircle, X,
} from 'lucide-react';
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
    <div className="flex flex-col gap-4 rounded-xl border border-primary/40 bg-card p-6 h-full">
      <div className="flex items-center gap-2 flex-wrap">
        <Sparkles className="h-4 w-4 text-primary shrink-0" />
        <span className="font-semibold text-foreground">ChatGPT / Codex</span>
        <span className="rounded-full bg-primary/15 px-2 py-0.5 text-[10px] font-medium text-primary whitespace-nowrap">
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
          <AlertCircle className="h-3.5 w-3.5 shrink-0" />{error}
        </p>
      )}
      <Button className="w-full" onClick={() => void handleConnect()} disabled={pending}>
        {pending
          ? <><Loader2 className="mr-2 h-4 w-4 animate-spin" />Waiting for browser...</>
          : 'Continue with ChatGPT'}
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
    <div className="flex flex-col gap-4 rounded-xl border border-border bg-card p-6 h-full">
      <div className="flex items-center gap-2">
        <Key className="h-4 w-4 text-muted-foreground shrink-0" />
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
                <CheckCircle2 className="h-3.5 w-3.5" />API key is valid
              </p>
            )}
            {error && (
              <p className="flex items-center gap-1.5 text-xs text-destructive">
                <AlertCircle className="h-3.5 w-3.5 shrink-0" />{error}
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
        {saving
          ? <><Loader2 className="mr-2 h-4 w-4 animate-spin" />Saving...</>
          : 'Use API Key'}
      </Button>
    </div>
  );
}

export function AddProviderModal({ open, onOpenChange, configuredProviderIds, onDone }: AddProviderModalProps) {
  const [faqOpen, setFaqOpen] = useState(false);
  const chatGPTConnected = configuredProviderIds.includes('openai');

  const handleDone = () => {
    onDone();
    onOpenChange(false);
  };

  return (
    <AlertDialog open={open} onOpenChange={onOpenChange}>
      <AlertDialogPortal>
        <AlertDialogOverlay />
        <AlertDialogContent className="max-w-2xl w-full p-0 overflow-hidden">
          {/* Header */}
          <div className="flex items-center justify-between border-b border-border px-6 py-4">
            <h2 className="text-base font-semibold">Add Provider</h2>
            <Button variant="ghost" size="icon" onClick={() => onOpenChange(false)}>
              <X className="h-4 w-4" />
            </Button>
          </div>

          {/* Body */}
          <div className="p-6 flex flex-col gap-5">
            <div className={cn('grid gap-4', chatGPTConnected ? 'grid-cols-1' : 'grid-cols-2')}>
              {!chatGPTConnected && <ChatGPTCard onDone={handleDone} />}
              <ApiKeyCard onDone={handleDone} />
            </div>

            {/* FAQ */}
            <Collapsible open={faqOpen} onOpenChange={setFaqOpen}>
              <CollapsibleTrigger className="flex w-full items-center justify-between gap-2 rounded-md px-1 py-1.5 text-xs text-muted-foreground hover:text-foreground transition-colors">
                <span className="flex items-center gap-1.5">
                  <HelpCircle className="h-3.5 w-3.5 shrink-0" />
                  Why can&apos;t I use my Claude Pro or Gemini subscription?
                </span>
                <ChevronDown className={cn('h-3.5 w-3.5 shrink-0 transition-transform', faqOpen && 'rotate-180')} />
              </CollapsibleTrigger>
              <CollapsibleContent className="overflow-hidden">
                <div className="rounded-md border border-border bg-card/50 p-4 mt-1 text-xs text-muted-foreground space-y-2">
                  <p>Both Anthropic and Google prohibit using their subscription OAuth tokens in third-party tools. Accounts have been suspended for this.</p>
                  <p>OpenAI explicitly allows ChatGPT plan tokens in third-party apps. For Claude or Gemini models, use an API key instead.</p>
                </div>
              </CollapsibleContent>
            </Collapsible>
          </div>
        </AlertDialogContent>
      </AlertDialogPortal>
    </AlertDialog>
  );
}
