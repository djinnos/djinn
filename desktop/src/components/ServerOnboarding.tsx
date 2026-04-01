import { useEffect, useState, useCallback } from 'react';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import {
  AlertCircleIcon,
  ComputerIcon,
  Loading02Icon,
  Wifi01Icon,
} from '@hugeicons/core-free-icons';
import { HugeiconsIcon } from '@hugeicons/react';
import logoSvg from '@/assets/logo.svg';
import {
  hasSavedConnectionMode,
  retryServerConnection,
  setConnectionMode,
  getServerStatus,
  saveSshHost,
  testSshConnection,
  type SshHost,
} from '@/tauri/commands';
import { HostEditor } from '@/components/ConnectionSettings';

type Phase =
  | { type: 'checking' }
  | { type: 'setup' }
  | { type: 'connecting'; label: string }
  | { type: 'error'; message: string }
  | { type: 'connected' };

export function ServerOnboarding({ children }: { children: React.ReactNode }) {
  const [phase, setPhase] = useState<Phase>({ type: 'checking' });
  const [hostEditorOpen, setHostEditorOpen] = useState(false);
  const [remoteUrlMode, setRemoteUrlMode] = useState(false);
  const [remoteUrl, setRemoteUrl] = useState('');

  // Poll server status to detect connection
  useEffect(() => {
    if (phase.type === 'connected') return;

    const poll = async () => {
      try {
        const status = await getServerStatus();
        if (status.is_healthy) {
          setPhase({ type: 'connected' });
        }
      } catch {
        // ignore polling errors
      }
    };

    const id = setInterval(poll, 2000);
    void poll();
    return () => clearInterval(id);
  }, [phase.type]);

  // On mount: check if we have a saved connection mode and try to auto-connect
  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        const hasSaved = await hasSavedConnectionMode();
        if (!hasSaved) {
          if (!cancelled) setPhase({ type: 'setup' });
          return;
        }

        // Try to auto-connect with saved mode
        if (!cancelled) setPhase({ type: 'connecting', label: 'Reconnecting...' });
        await retryServerConnection();
        // If we get here, connection succeeded — polling will pick it up
      } catch (err) {
        if (!cancelled) {
          setPhase({
            type: 'error',
            message: err instanceof Error ? err.message : 'Failed to connect to server',
          });
        }
      }
    })();
    return () => { cancelled = true; };
  }, []);

  const handleLocalSetup = useCallback(async () => {
    setPhase({ type: 'connecting', label: 'Starting local server...' });
    try {
      await setConnectionMode({ type: 'daemon' });
      await retryServerConnection();
      // Polling will detect healthy status
    } catch (err) {
      setPhase({
        type: 'error',
        message: err instanceof Error ? err.message : 'Failed to start local server',
      });
    }
  }, []);

  const handleSshHostSave = useCallback(async (host: SshHost) => {
    await saveSshHost(host);
    setHostEditorOpen(false);
    setPhase({ type: 'connecting', label: `Connecting to ${host.label || host.hostname}...` });
    try {
      await setConnectionMode({ type: 'ssh', host_id: host.id });
      await retryServerConnection();
    } catch (err) {
      setPhase({
        type: 'error',
        message: err instanceof Error ? err.message : 'Failed to connect via SSH',
      });
    }
  }, []);

  const handleRemoteUrl = useCallback(async () => {
    const url = remoteUrl.trim();
    if (!url) return;
    setPhase({ type: 'connecting', label: `Connecting to ${url}...` });
    try {
      await setConnectionMode({ type: 'remote', url });
      await retryServerConnection();
    } catch (err) {
      setPhase({
        type: 'error',
        message: err instanceof Error ? err.message : 'Failed to connect to remote server',
      });
    }
  }, [remoteUrl]);

  const handleRetry = useCallback(async () => {
    setPhase({ type: 'connecting', label: 'Retrying...' });
    try {
      await retryServerConnection();
    } catch (err) {
      setPhase({
        type: 'error',
        message: err instanceof Error ? err.message : 'Failed to connect',
      });
    }
  }, []);

  if (phase.type === 'connected') {
    return <>{children}</>;
  }

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

        {/* Checking / Connecting */}
        {(phase.type === 'checking' || phase.type === 'connecting') && (
          <div className="flex flex-col items-center gap-4">
            <HugeiconsIcon icon={Loading02Icon} size={28} className="animate-spin text-muted-foreground" />
            <p className="text-sm text-muted-foreground">
              {phase.type === 'checking' ? 'Checking server...' : phase.label}
            </p>
          </div>
        )}

        {/* Error */}
        {phase.type === 'error' && (
          <>
            <div className="text-center space-y-2">
              <h2 className="text-2xl font-semibold">Connection failed</h2>
              <p className="flex items-center justify-center gap-2 text-sm text-destructive">
                <HugeiconsIcon icon={AlertCircleIcon} size={16} className="shrink-0" />
                {phase.message}
              </p>
            </div>
            <div className="flex gap-3">
              <Button variant="outline" onClick={() => setPhase({ type: 'setup' })}>
                Change setup
              </Button>
              <Button onClick={() => void handleRetry()}>
                Retry
              </Button>
            </div>
          </>
        )}

        {/* Setup */}
        {phase.type === 'setup' && (
          <>
            <div className="text-center space-y-2">
              <h2 className="text-2xl font-semibold">Set up your server</h2>
              <p className="text-base text-muted-foreground">
                Djinn needs a server to manage tasks, agents, and memory.
              </p>
            </div>

            <div className="grid w-full grid-cols-2 gap-5">
              {/* Local Server Card */}
              <div className="relative flex flex-col gap-4 rounded-2xl border border-primary/40 bg-gradient-to-br from-primary/[0.06] to-transparent p-7 h-full overflow-hidden">
                <div className="pointer-events-none absolute -right-8 -top-8 h-28 w-28 rounded-full bg-primary/20 blur-3xl" />
                <div className="pointer-events-none absolute -left-6 -bottom-6 h-20 w-20 rounded-full bg-primary/10 blur-3xl" />

                <div className="flex items-center gap-3">
                  <div className="flex h-10 w-10 shrink-0 items-center justify-center rounded-xl bg-primary/15">
                    <HugeiconsIcon icon={ComputerIcon} size={20} className="text-primary" />
                  </div>
                  <div>
                    <h3 className="text-base font-semibold text-foreground">Local Server</h3>
                    <p className="text-xs text-muted-foreground">Run on this machine</p>
                  </div>
                </div>

                <p className="text-sm leading-relaxed text-muted-foreground flex-1">
                  Run the Djinn server locally. The server binary will be downloaded automatically if needed.
                </p>

                <span className="inline-flex self-start rounded-full bg-green-500/15 px-3 py-1 text-xs font-medium text-green-400">
                  Recommended
                </span>

                <Button size="lg" className="w-full text-sm" onClick={() => void handleLocalSetup()}>
                  Start Local Server
                </Button>
              </div>

              {/* Remote Server Card */}
              <div className="flex flex-col gap-4 rounded-2xl border border-border bg-card p-7 h-full">
                <div className="flex items-center gap-3">
                  <div className="flex h-10 w-10 shrink-0 items-center justify-center rounded-xl bg-muted">
                    <HugeiconsIcon icon={Wifi01Icon} size={20} className="text-muted-foreground" />
                  </div>
                  <div>
                    <h3 className="text-base font-semibold text-foreground">Remote Server</h3>
                    <p className="text-xs text-muted-foreground">SSH or direct URL</p>
                  </div>
                </div>

                <p className="text-sm leading-relaxed text-muted-foreground flex-1">
                  Connect to a Djinn server running on a remote machine via SSH tunnel or direct URL.
                </p>

                <div className="flex flex-col gap-2">
                  <Button
                    variant="outline"
                    size="lg"
                    className="w-full text-sm"
                    onClick={() => setHostEditorOpen(true)}
                  >
                    Connect via SSH
                  </Button>
                  <Button
                    variant="ghost"
                    size="sm"
                    className="w-full text-sm text-muted-foreground"
                    onClick={() => setRemoteUrlMode(!remoteUrlMode)}
                  >
                    {remoteUrlMode ? 'Hide URL input' : 'Or enter a direct URL'}
                  </Button>
                  {remoteUrlMode && (
                    <div className="flex gap-2">
                      <Input
                        placeholder="http://192.168.1.100:8372"
                        value={remoteUrl}
                        onChange={(e) => setRemoteUrl(e.target.value)}
                        className="flex-1"
                      />
                      <Button
                        variant="outline"
                        disabled={!remoteUrl.trim()}
                        onClick={() => void handleRemoteUrl()}
                      >
                        Connect
                      </Button>
                    </div>
                  )}
                </div>
              </div>
            </div>

            {/* SSH Host Editor Modal */}
            <HostEditor
              open={hostEditorOpen}
              onOpenChange={setHostEditorOpen}
              host={null}
              onSave={handleSshHostSave}
              onTest={testSshConnection}
            />
          </>
        )}
      </div>
    </main>
  );
}
