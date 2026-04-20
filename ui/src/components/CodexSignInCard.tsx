import { useState } from 'react';
import {
  AlertCircleIcon,
  CheckmarkCircle04Icon,
  Copy01Icon,
  LinkForwardIcon,
  Loading02Icon,
  SparklesIcon,
} from '@hugeicons/core-free-icons';
import { HugeiconsIcon } from '@hugeicons/react';

import { Button } from '@/components/ui/button';
import { startProviderOAuth } from '@/api/server';
import { showToast } from '@/lib/toast';
import { cn } from '@/lib/utils';

type CardPhase =
  | { kind: 'idle' }
  | {
      kind: 'pending';
      userCode: string;
      verificationUri: string;
      verificationUriComplete: string;
      expiresInSecs: number;
    }
  | { kind: 'error'; message: string }
  | { kind: 'just_connected' };

interface Props {
  /**
   * Caller-provided flag indicating that `chatgpt_codex` already has a live
   * token in the vault. When true — and the user hasn't started a new sign-in
   * attempt — the card renders a compact "connected" state instead of the
   * sign-in CTA.
   */
  alreadyConnected?: boolean;
  /** Invoked after a successful sign-in so the parent can refresh state. */
  onConnected?: () => void;
  className?: string;
}

export function CodexSignInCard({ alreadyConnected, onConnected, className }: Props) {
  const [phase, setPhase] = useState<CardPhase>({ kind: 'idle' });
  // Show the green "connected" panel when either the parent told us we're
  // already connected (and the user hasn't interacted) or the flow just
  // completed in this session.
  const showConnected =
    phase.kind === 'just_connected' || (phase.kind === 'idle' && alreadyConnected);

  const handleConnect = async () => {
    setPhase({ kind: 'idle' });
    try {
      const result = await startProviderOAuth('openai');
      if (result.success) {
        setPhase({ kind: 'just_connected' });
        onConnected?.();
        return;
      }
      if (
        result.pending &&
        result.user_code &&
        result.verification_uri &&
        result.verification_uri_complete
      ) {
        setPhase({
          kind: 'pending',
          userCode: result.user_code,
          verificationUri: result.verification_uri,
          verificationUriComplete: result.verification_uri_complete,
          expiresInSecs: result.expires_in ?? 900,
        });
        return;
      }
      setPhase({ kind: 'error', message: result.error ?? 'OAuth flow failed' });
    } catch (error) {
      setPhase({
        kind: 'error',
        message: error instanceof Error ? error.message : 'OAuth flow failed',
      });
    }
  };

  const handleCopyCode = async (code: string) => {
    try {
      await navigator.clipboard.writeText(code);
      showToast.success('Code copied');
    } catch {
      showToast.error('Could not copy', { description: 'Copy the code manually.' });
    }
  };

  return (
    <div
      className={cn(
        'relative flex flex-col gap-4 rounded-2xl border border-primary/40 bg-gradient-to-br from-primary/[0.06] to-transparent p-7 h-full overflow-hidden',
        className,
      )}
    >
      <div className="pointer-events-none absolute -right-8 -top-8 h-28 w-28 rounded-full bg-primary/20 blur-3xl" />
      <div className="pointer-events-none absolute -left-6 -bottom-6 h-20 w-20 rounded-full bg-primary/10 blur-3xl" />

      <div className="flex items-center gap-3">
        <div className="flex h-10 w-10 shrink-0 items-center justify-center rounded-xl bg-primary/15">
          <HugeiconsIcon icon={SparklesIcon} size={20} className="text-primary" />
        </div>
        <div>
          <h3 className="text-base font-semibold text-foreground">ChatGPT / Codex</h3>
          <p className="text-xs text-muted-foreground">Sign in with a device code</p>
        </div>
      </div>

      {phase.kind === 'idle' && !showConnected && (
        <>
          <p className="text-sm leading-relaxed text-muted-foreground flex-1">
            Sign in with your ChatGPT Plus, Pro, or Team account. Works from any browser — no
            local port-forwarding required.
          </p>
          <span className="inline-flex self-start rounded-full bg-green-500/15 px-3 py-1 text-xs font-medium text-green-400">
            No API key needed
          </span>
          <Button size="lg" className="w-full text-sm" onClick={() => void handleConnect()}>
            Continue with ChatGPT
          </Button>
        </>
      )}

      {phase.kind === 'pending' && (
        <>
          <div className="flex flex-col gap-3 flex-1">
            <p className="text-sm leading-relaxed text-muted-foreground">
              Open the sign-in page in your browser and enter this code:
            </p>
            <div className="flex items-center gap-2">
              <code className="flex-1 rounded-lg border border-border bg-card px-4 py-3 text-center text-2xl font-mono font-semibold tracking-widest text-foreground">
                {phase.userCode}
              </code>
              <Button
                type="button"
                variant="outline"
                size="lg"
                onClick={() => void handleCopyCode(phase.userCode)}
                aria-label="Copy code"
              >
                <HugeiconsIcon icon={Copy01Icon} size={18} />
              </Button>
            </div>
            <p className="flex items-center gap-2 text-xs text-muted-foreground">
              <HugeiconsIcon icon={Loading02Icon} size={14} className="animate-spin shrink-0" />
              Waiting for you to complete sign-in
              {phase.expiresInSecs ? ` (expires in ${Math.floor(phase.expiresInSecs / 60)} min)` : ''}…
            </p>
          </div>
          <a
            href={phase.verificationUriComplete}
            target="_blank"
            rel="noopener noreferrer"
            className="inline-flex items-center justify-center gap-2 rounded-md bg-primary px-4 py-2.5 text-sm font-medium text-primary-foreground shadow hover:bg-primary/90 transition-colors"
          >
            Open sign-in page
            <HugeiconsIcon icon={LinkForwardIcon} size={16} />
          </a>
        </>
      )}

      {showConnected && (
        <>
          <p className="text-sm leading-relaxed text-muted-foreground flex-1">
            Your ChatGPT account is signed in. Djinn keeps your tokens refreshed automatically.
          </p>
          <span className="inline-flex self-start items-center gap-1.5 rounded-full bg-green-500/15 px-3 py-1 text-xs font-medium text-green-400">
            <HugeiconsIcon icon={CheckmarkCircle04Icon} size={14} />
            Connected
          </span>
          <Button
            variant="outline"
            size="lg"
            className="w-full text-sm"
            onClick={() => void handleConnect()}
          >
            Reconnect
          </Button>
        </>
      )}

      {phase.kind === 'error' && (
        <>
          <p className="flex items-start gap-2 text-sm text-destructive">
            <HugeiconsIcon icon={AlertCircleIcon} size={16} className="shrink-0 mt-0.5" />
            <span>{phase.message}</span>
          </p>
          <Button size="lg" className="w-full text-sm" onClick={() => void handleConnect()}>
            Try again
          </Button>
        </>
      )}
    </div>
  );
}
