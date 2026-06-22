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
import {
  Popover,
  PopoverContent,
  PopoverTrigger,
} from '@/components/ui/popover';
import { startProviderOAuth, removeProviderFull } from '@/api/server';
import { showToast } from '@/lib/toast';

type RowPhase =
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
   * attempt — the row renders its compact "connected" state (Reconnect /
   * Disconnect) instead of the sign-in CTA.
   */
  alreadyConnected?: boolean;
  /**
   * Set when the codex credential was rejected (a 401 during a run) and marked
   * revoked server-side. Surfaces a "Disconnected — <reason>" hint on the row.
   * Persisted server-side, so it shows on a fresh page load too.
   */
  revokedReason?: string;
  /** Invoked after a successful sign-in/disconnect so the parent can refresh. */
  onConnected?: () => void;
}

/**
 * ChatGPT / Codex as a compact subscription ROW (matching the other connected
 * subscriptions) instead of a full-bleed card. The row carries the device-code
 * sign-in when disconnected, and Reconnect + Disconnect when connected — the
 * device-code flow + credential management live in an inline popover so the row
 * itself stays one line.
 */
export function CodexSignInRow({ alreadyConnected, revokedReason, onConnected }: Props) {
  const [phase, setPhase] = useState<RowPhase>({ kind: 'idle' });
  const [removing, setRemoving] = useState(false);
  const [open, setOpen] = useState(false);

  // Show the connected state when either the parent told us we're already
  // connected (and the user hasn't interacted) or the flow just completed.
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

  // Codex connects under the `openai` provider (merge_into), but its credential
  // is stored as `chatgpt_codex` — disconnect by that own provider id.
  const handleRemove = async () => {
    setRemoving(true);
    try {
      await removeProviderFull('chatgpt_codex');
      setPhase({ kind: 'idle' });
      setOpen(false);
      showToast.success('ChatGPT disconnected', {
        description: 'You can sign in again to reconnect.',
      });
      onConnected?.();
    } catch (error) {
      showToast.error('Could not disconnect', {
        description: error instanceof Error ? error.message : 'Unknown error',
      });
    } finally {
      setRemoving(false);
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

  const subtitle = showConnected
    ? 'Connected · personal subscription'
    : revokedReason
      ? `Disconnected — ${revokedReason}`
      : 'Sign in with a device code · no API key needed';

  return (
    <li className="flex items-center justify-between gap-3 rounded-lg border border-border bg-card px-4 py-3">
      <div className="flex min-w-0 items-center gap-2.5">
        {showConnected ? (
          <HugeiconsIcon
            icon={CheckmarkCircle04Icon}
            size={16}
            className="shrink-0 text-green-500"
          />
        ) : (
          <HugeiconsIcon icon={SparklesIcon} size={16} className="shrink-0 text-primary" />
        )}
        <div className="min-w-0">
          <div className="truncate text-sm font-medium text-foreground">ChatGPT / Codex</div>
          <div
            className={
              'truncate text-xs ' +
              (revokedReason && !showConnected ? 'text-destructive' : 'text-muted-foreground')
            }
          >
            {subtitle}
          </div>
        </div>
      </div>

      <Popover
        open={open}
        onOpenChange={(next) => {
          setOpen(next);
          if (!next && phase.kind !== 'pending') setPhase({ kind: 'idle' });
        }}
      >
        {showConnected ? (
          <PopoverTrigger render={<Button size="sm" variant="outline" />}>
            Manage
          </PopoverTrigger>
        ) : (
          <PopoverTrigger
            render={<Button size="sm" />}
            onClick={() => {
              // Kick off the device-code flow as the popover opens.
              if (phase.kind === 'idle') void handleConnect();
            }}
          >
            Sign in
          </PopoverTrigger>
        )}

        <PopoverContent className="w-80">
          <div className="flex flex-col gap-3">
            <div>
              <div className="text-sm font-semibold text-foreground">ChatGPT / Codex</div>
              <p className="text-xs text-muted-foreground">
                Sign in with your ChatGPT Plus, Pro, or Team account from any browser — no
                local port-forwarding required.
              </p>
            </div>

            {phase.kind === 'pending' && (
              <div className="flex flex-col gap-2">
                <p className="text-xs text-muted-foreground">
                  Open the sign-in page and enter this code:
                </p>
                <div className="flex items-center gap-2">
                  <code className="flex-1 rounded-lg border border-border bg-card px-3 py-2 text-center text-lg font-mono font-semibold tracking-widest text-foreground">
                    {phase.userCode}
                  </code>
                  <Button
                    type="button"
                    variant="outline"
                    size="icon"
                    onClick={() => void handleCopyCode(phase.userCode)}
                    aria-label="Copy code"
                  >
                    <HugeiconsIcon icon={Copy01Icon} size={16} />
                  </Button>
                </div>
                <p className="flex items-center gap-2 text-xs text-muted-foreground">
                  <HugeiconsIcon icon={Loading02Icon} size={14} className="shrink-0 animate-spin" />
                  Waiting for sign-in
                  {phase.expiresInSecs
                    ? ` (expires in ${Math.floor(phase.expiresInSecs / 60)} min)`
                    : ''}
                  …
                </p>
                <a
                  href={phase.verificationUriComplete}
                  target="_blank"
                  rel="noopener noreferrer"
                  className="inline-flex items-center justify-center gap-2 rounded-md bg-primary px-3 py-2 text-sm font-medium text-primary-foreground shadow transition-colors hover:bg-primary/90"
                >
                  Open sign-in page
                  <HugeiconsIcon icon={LinkForwardIcon} size={16} />
                </a>
              </div>
            )}

            {phase.kind === 'error' && (
              <>
                <p className="flex items-start gap-2 text-xs text-destructive">
                  <HugeiconsIcon icon={AlertCircleIcon} size={14} className="mt-0.5 shrink-0" />
                  <span>{phase.message}</span>
                </p>
                <Button size="sm" className="w-full" onClick={() => void handleConnect()}>
                  Try again
                </Button>
              </>
            )}

            {phase.kind !== 'pending' && phase.kind !== 'error' && !showConnected && (
              <Button size="sm" className="w-full" onClick={() => void handleConnect()}>
                Continue with ChatGPT
              </Button>
            )}

            {showConnected && (
              <div className="flex flex-col gap-2">
                <span className="inline-flex w-fit items-center gap-1.5 rounded-full bg-green-500/15 px-3 py-1 text-xs font-medium text-green-400">
                  <HugeiconsIcon icon={CheckmarkCircle04Icon} size={14} />
                  Connected
                </span>
                <Button
                  size="sm"
                  variant="outline"
                  className="w-full"
                  onClick={() => void handleConnect()}
                >
                  Reconnect
                </Button>
                <Button
                  size="sm"
                  variant="ghost"
                  disabled={removing}
                  className="w-full text-destructive hover:text-destructive"
                  onClick={() => void handleRemove()}
                >
                  {removing ? 'Removing…' : 'Remove / Disconnect'}
                </Button>
              </div>
            )}
          </div>
        </PopoverContent>
      </Popover>
    </li>
  );
}
